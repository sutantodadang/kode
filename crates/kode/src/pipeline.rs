use std::path::Path;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use kode_agent::Agent;
use kode_context::{CompiledContext, ContextCompiler, ContextRequest, ContextSource};
use kode_core::CancellationToken;
use kode_core::config::{AgentConfig, IngatConfig, KodeConfig, PermissionMode};
use kode_core::event::{EventBus, KodeEvent, NoteSource, TaskStep};
use kode_intel::{CodeIntelligence, ZindeksAdapter};
use kode_memory::{EngineeringMemory, IngatAdapter, RememberTool};
use kode_model::{OpenAiModel, OpenAiOptions, Usage};
use kode_tools::ToolContext;
use kode_tools::permission::PermissionHandler;
use kode_tools::registry::{ToolRegistry, ToolRuntime};

/// Appended to the task text for the plan-mode turn (see
/// [`run_plan_phase`]). The turn runs under an empty tool registry, so this
/// also tells the model plainly that it has nothing to call.
const PLAN_INSTRUCTION: &str = "Before making any changes, write a concise numbered plan for \
accomplishing this task: the concrete steps you would take and which files you would touch. \
Do not write code. You have no tools available for this turn — just describe the plan, then stop.";

/// Set once this process has made its one autostart attempt for the Ingat
/// service (successful or not). Guards against re-attempting on every task
/// within a long-lived `kode` process (e.g. the TUI running many turns).
static INGAT_AUTOSTART_ATTEMPTED: OnceLock<()> = OnceLock::new();

/// Machine-readable result of a completed pipeline run. UIs may keep using
/// [`KodeEvent`] for live rendering, while headless callers use this value to
/// decide whether the task actually succeeded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskOutcome {
    pub status: TaskStatus,
    pub mutated: bool,
    pub verification: VerificationStatus,
    pub repair_attempted: bool,
    pub iterations: u32,
    pub tool_calls: u32,
    pub usage: Usage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskStatus {
    Completed,
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerificationStatus {
    NotNeeded,
    Verified,
    Failed,
    NoChecks,
}

impl TaskOutcome {
    /// A production-safe success policy: changed files must have passed at
    /// least one real verification check. Skipped/missing checks never pass.
    pub fn is_success(&self) -> bool {
        self.status == TaskStatus::Completed
            && matches!(
                self.verification,
                VerificationStatus::NotNeeded | VerificationStatus::Verified
            )
    }
}

/// Runs one agentic task end-to-end: model/config setup, code intelligence
/// and engineering memory binding, context compilation, the agent loop, and
/// post-edit verification with a single retry. This is the single code path
/// shared by `kode exec` (headless) and the TUI — it communicates *only*
/// through `events`, never via stdout/stderr directly.
#[allow(clippy::too_many_arguments)]
pub async fn run_task(
    task: &str,
    cwd: &Path,
    config: &KodeConfig,
    events: EventBus,
    handler: Arc<dyn PermissionHandler>,
    cancel: CancellationToken,
    history: &[kode_agent::HistoryTurn],
    plan_mode: bool,
) -> anyhow::Result<TaskOutcome> {
    if config.model.model.is_empty() {
        anyhow::bail!("set model.model in .kode/config.toml");
    }

    let model: Arc<dyn kode_model::Model> = match config.model.provider.as_str() {
        "openai" => {
            let api_key = std::env::var("OPENAI_API_KEY")
                .or_else(|_| std::env::var("KODE_API_KEY"))
                .map_err(|_| anyhow::anyhow!("set OPENAI_API_KEY to run `kode exec`"))?;

            let mut opts = OpenAiOptions {
                api_key,
                model: config.model.model.clone(),
                ..Default::default()
            };
            if let Ok(base_url) = std::env::var("OPENAI_BASE_URL") {
                opts.base_url = base_url;
            }
            Arc::new(OpenAiModel::new(opts)) as Arc<dyn kode_model::Model>
        }
        "codex" => {
            let auth_path = kode_model::codex::default_auth_path()
                .ok_or_else(|| anyhow::anyhow!("cannot resolve home directory for codex auth"))?;
            let auth = kode_model::codex::load(&auth_path).map_err(|e| anyhow::anyhow!("{e}"))?;

            if auth.auth_mode == "apikey" {
                let api_key = auth.api_key.clone().ok_or_else(|| {
                    anyhow::anyhow!(
                        "codex auth.json has auth_mode=apikey but no OPENAI_API_KEY — run: kode auth login codex"
                    )
                })?;
                let opts = OpenAiOptions {
                    api_key,
                    model: config.model.model.clone(),
                    ..Default::default()
                };
                Arc::new(OpenAiModel::new(opts)) as Arc<dyn kode_model::Model>
            } else {
                let codex_model =
                    kode_model::CodexModel::new(auth_path, config.model.model.clone())
                        .map_err(|e| anyhow::anyhow!("{e}"))?;
                Arc::new(codex_model) as Arc<dyn kode_model::Model>
            }
        }
        "opencode-go" | "opencode" | "kilo" | "lmstudio" => {
            let auth_path = kode_model::opencode::default_auth_path().ok_or_else(|| {
                anyhow::anyhow!("cannot resolve home directory for opencode auth")
            })?;
            // Base URLs come from the builtin gateway table only; no reads of
            // another tool's config.
            let opencode_model = kode_model::opencode::resolve(
                &config.model.provider,
                config.model.model.clone(),
                &auth_path,
                None,
            )
            .map_err(|e| anyhow::anyhow!("{e}"))?;
            Arc::new(opencode_model) as Arc<dyn kode_model::Model>
        }
        "anthropic" => {
            let auth_path = kode_model::anthropic::default_auth_path().ok_or_else(|| {
                anyhow::anyhow!("cannot resolve home directory for anthropic auth")
            })?;
            let anthropic_model =
                kode_model::AnthropicModel::new(auth_path, config.model.model.clone())
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
            Arc::new(anthropic_model) as Arc<dyn kode_model::Model>
        }
        other => anyhow::bail!(
            "provider {other} not supported yet (supported: openai, anthropic, codex, opencode-go, opencode, kilo, lmstudio)"
        ),
    };

    let ctx = ToolContext {
        workspace_root: cwd.to_path_buf(),
        cancel,
    };

    // Keep a concrete handle alongside the `dyn CodeIntelligence` one so we
    // can call `ensure_bound` again (as an incremental refresh) after edits.
    let mut zindeks_adapter: Option<Arc<ZindeksAdapter>> = None;
    let intel: Option<Arc<dyn CodeIntelligence>> = if config.zindeks.enabled {
        match ZindeksAdapter::connect(&config.zindeks, cwd).await {
            Ok(adapter) => {
                let adapter = Arc::new(adapter);
                match adapter.ensure_bound().await {
                    Ok(()) => {
                        zindeks_adapter = Some(adapter.clone());
                        Some(adapter as Arc<dyn CodeIntelligence>)
                    }
                    Err(e) => {
                        events.emit(KodeEvent::SourcedNote {
                            text: format!("code intelligence unavailable: {e}"),
                            source: NoteSource::Zindeks,
                        });
                        None
                    }
                }
            }
            Err(e) => {
                events.emit(KodeEvent::SourcedNote {
                    text: format!("code intelligence unavailable: {e}"),
                    source: NoteSource::Zindeks,
                });
                None
            }
        }
    } else {
        None
    };

    let memory: Option<Arc<dyn EngineeringMemory>> = if config.ingat.enabled {
        let adapter = IngatAdapter::new(&config.ingat);
        match tokio::time::timeout(Duration::from_secs(3), adapter.health()).await {
            Ok(Ok(())) => Some(Arc::new(adapter) as Arc<dyn EngineeringMemory>),
            Ok(Err(e)) => {
                events.emit(KodeEvent::SourcedNote {
                    text: format!("engineering memory unavailable: {e}"),
                    source: NoteSource::Ingat,
                });
                maybe_autostart_ingat(&config.ingat, &events).await
            }
            Err(_) => {
                events.emit(KodeEvent::SourcedNote {
                    text: "engineering memory unavailable: request timed out".to_string(),
                    source: NoteSource::Ingat,
                });
                maybe_autostart_ingat(&config.ingat, &events).await
            }
        }
    } else {
        None
    };

    let mut registry = ToolRegistry::with_builtins();
    if let Some(mem) = &memory {
        let repository = cwd
            .file_name()
            .map(|name| name.to_string_lossy().to_string());
        registry.register(Arc::new(RememberTool::new(mem.clone(), repository)));
    }

    // Generic external MCP servers (kept architecturally separate from the
    // first-class Zindeks/Ingat integrations above). `_mcp_manager` owns the
    // spawned child processes and must outlive the agent run.
    let _mcp_manager = if !config.mcp.servers.is_empty() {
        let mut notes = Vec::new();
        let manager = kode_mcp::McpManager::connect_all(&config.mcp.servers, &mut notes).await;
        for text in notes {
            events.emit(KodeEvent::Note { text });
        }
        for handle in &manager.handles {
            for tool in &handle.tools {
                registry.register(tool.clone());
            }
        }
        Some(manager)
    } else {
        None
    };

    let tools = ToolRuntime::new(registry, config.permissions.default_mode, handler.clone());
    let effort = if config.model.effort.is_empty() {
        None
    } else {
        Some(config.model.effort.clone())
    };
    let agent =
        Agent::new(model.clone(), tools, events.clone(), &config.agent).with_effort(effort.clone());

    events.emit(KodeEvent::ContextCompilationStarted);
    let compiler = ContextCompiler::new(intel, memory, config.agent.context_budget_tokens as usize);
    let compiled = compiler
        .compile(
            &ContextRequest {
                task: task.to_string(),
                working_set: vec![],
            },
            cwd,
        )
        .await;
    events.emit(KodeEvent::ContextCompiled {
        token_estimate: compiled.token_estimate(),
        sections: compiled.sections.len(),
    });
    events.emit(knowledge_from(
        &compiled,
        config.agent.context_budget_tokens as usize,
    ));
    events.emit(KodeEvent::Note {
        text: compiled.summary_line(),
    });
    events.emit(KodeEvent::TaskProgress {
        step: TaskStep::Understand,
        done: true,
    });

    let (kept_history, history_truncated) =
        kode_agent::select_history(history, config.agent.history_budget_tokens as usize);
    if history_truncated {
        events.emit(KodeEvent::Note {
            text: "(older conversation truncated)".to_string(),
        });
    }

    // The prompt actually sent to the exec turn below: the original `task`
    // unless plan mode swaps in the approved-plan-injected version.
    let mut exec_task = task.to_string();

    if plan_mode {
        let plan_result = run_plan_phase(
            model.clone(),
            &events,
            handler.clone(),
            &config.agent,
            effort.clone(),
            task,
            compiled.render().as_deref(),
            kept_history,
            history_truncated,
            &ctx,
        )
        .await?;

        match plan_result {
            PlanOutcome::Rejected { outcome } => {
                events.emit(KodeEvent::Note {
                    text: "plan rejected — task cancelled".to_string(),
                });
                events.emit(KodeEvent::TaskFinished {
                    iterations: outcome.iterations,
                    tool_calls: outcome.tool_calls,
                    input_tokens: outcome.usage.input_tokens,
                    output_tokens: outcome.usage.output_tokens,
                });
                return Ok(TaskOutcome {
                    status: TaskStatus::Cancelled,
                    mutated: false,
                    verification: VerificationStatus::NotNeeded,
                    repair_attempted: false,
                    iterations: outcome.iterations,
                    tool_calls: outcome.tool_calls,
                    usage: outcome.usage,
                });
            }
            PlanOutcome::Approved { effective_task, .. } => {
                events.emit(KodeEvent::Note {
                    text: "plan approved — executing".to_string(),
                });
                events.emit(KodeEvent::TaskProgress {
                    step: TaskStep::Plan,
                    done: true,
                });
                exec_task = effective_task;
            }
        }
    }

    let outcome1 = agent
        .run_with_context(
            &exec_task,
            compiled.render().as_deref(),
            kept_history,
            history_truncated,
            &ctx,
        )
        .await
        .map_err(|err| anyhow::anyhow!(err))?;

    events.emit(KodeEvent::TaskProgress {
        step: TaskStep::Change,
        done: outcome1.mutated,
    });

    // outcome2 is only Some when a retry ran (i.e. the first verification
    // failed). Metrics and the mutated flag must aggregate across both runs
    // — see `combine_outcomes`.
    let mut outcome2: Option<kode_agent::AgentOutcome> = None;
    let mut verification = VerificationStatus::NotNeeded;

    if outcome1.mutated {
        let profile = kode_verify::detect(cwd);
        events.emit(KodeEvent::VerificationStarted);
        let report = kode_verify::run_verification(cwd, &profile, &ctx.cancel).await;
        emit_verify_steps(&events, &report);
        let verdict = verification_verdict(report.ok, report.ran_any());
        verification = verification_status(verdict);
        events.emit(KodeEvent::VerificationFinished {
            ok: verdict == Verdict::Verified,
        });
        events.emit(KodeEvent::TaskProgress {
            step: TaskStep::Verify,
            done: verdict == Verdict::Verified,
        });

        if verdict == Verdict::NoChecks {
            events.emit(KodeEvent::Note {
                text: "no verification checks for this project — changes are unverified"
                    .to_string(),
            });
        }

        if verdict == Verdict::Failed {
            events.emit(KodeEvent::Note {
                text: "verification failed — asking agent to fix".to_string(),
            });
            let retry_task = format!(
                "Verification failed after your previous changes. Fix the failures, then stop.\n\n{}\n\nOriginal task: {}",
                report.render(),
                task
            );

            // Repair must see the workspace produced by the first run, not
            // the pre-edit snapshot. Refresh code intelligence first (even
            // when a watcher exists, since it may not have observed the edit
            // yet), then compile a new git/intel/memory context.
            if let Some(adapter) = zindeks_adapter.as_ref() {
                match adapter.ensure_bound().await {
                    Ok(()) => events.emit(KodeEvent::SourcedNote {
                        text: "zindeks index refreshed before repair".to_string(),
                        source: NoteSource::Zindeks,
                    }),
                    Err(e) => events.emit(KodeEvent::SourcedNote {
                        text: format!("zindeks pre-repair refresh failed (non-fatal): {e}"),
                        source: NoteSource::Zindeks,
                    }),
                }
            }
            events.emit(KodeEvent::ContextCompilationStarted);
            let repair_context = compiler
                .compile(
                    &ContextRequest {
                        task: retry_task.clone(),
                        working_set: vec![],
                    },
                    cwd,
                )
                .await;
            events.emit(KodeEvent::ContextCompiled {
                token_estimate: repair_context.token_estimate(),
                sections: repair_context.sections.len(),
            });

            let retry_outcome = agent
                .run_with_context(
                    &retry_task,
                    repair_context.render().as_deref(),
                    kept_history,
                    history_truncated,
                    &ctx,
                )
                .await
                .map_err(|err| anyhow::anyhow!(err))?;

            let mutated_any = outcome1.mutated || retry_outcome.mutated;
            let mut retry_report = report;

            if mutated_any {
                let profile = kode_verify::detect(cwd);
                events.emit(KodeEvent::VerificationStarted);
                retry_report = kode_verify::run_verification(cwd, &profile, &ctx.cancel).await;
                emit_verify_steps(&events, &retry_report);
                let verdict2 = verification_verdict(retry_report.ok, retry_report.ran_any());
                verification = verification_status(verdict2);
                events.emit(KodeEvent::VerificationFinished {
                    ok: verdict2 == Verdict::Verified,
                });
                events.emit(KodeEvent::TaskProgress {
                    step: TaskStep::Verify,
                    done: verdict2 == Verdict::Verified,
                });
                if verdict2 == Verdict::NoChecks {
                    events.emit(KodeEvent::Note {
                        text: "no verification checks for this project — changes are unverified"
                            .to_string(),
                    });
                }
            }
            events.emit(KodeEvent::Note {
                text: retry_report.summary_line(),
            });

            outcome2 = Some(retry_outcome);
        }
    }

    let (iterations, tool_calls, input_tokens, output_tokens, mutated_any) = match &outcome2 {
        Some(o2) => combine_outcomes(&outcome1, o2),
        None => (
            outcome1.iterations,
            outcome1.tool_calls,
            outcome1.usage.input_tokens,
            outcome1.usage.output_tokens,
            outcome1.mutated,
        ),
    };

    if let Some(adapter) = zindeks_adapter.as_ref()
        && mutated_any
    {
        if adapter.watching() {
            // The spawned zindeks server has its own poll-watcher running
            // (ZINDEKS_WATCH=1) and will pick up the mutation on its own —
            // no need for Kode to trigger an explicit refresh.
            events.emit(KodeEvent::SourcedNote {
                text: "zindeks watcher active — index updates automatically".to_string(),
                source: NoteSource::Zindeks,
            });
        } else {
            match adapter.ensure_bound().await {
                Ok(()) => events.emit(KodeEvent::SourcedNote {
                    text: "zindeks index refreshed".to_string(),
                    source: NoteSource::Zindeks,
                }),
                Err(e) => events.emit(KodeEvent::SourcedNote {
                    text: format!("zindeks refresh failed (non-fatal): {e}"),
                    source: NoteSource::Zindeks,
                }),
            }
        }
    }

    events.emit(KodeEvent::TaskFinished {
        iterations,
        tool_calls,
        input_tokens,
        output_tokens,
    });

    Ok(TaskOutcome {
        status: TaskStatus::Completed,
        mutated: mutated_any,
        verification,
        repair_attempted: outcome2.is_some(),
        iterations,
        tool_calls,
        usage: Usage {
            input_tokens,
            output_tokens,
        },
    })
}

/// Outcome of [`run_plan_phase`]: the human's approve/reject answer to
/// "execute this plan?", carrying the plan turn's own `AgentOutcome` either
/// way — `run_task` reports it as the `TaskFinished` counters when the plan
/// is rejected (the exec turn never ran, so it's the only usage there is).
enum PlanOutcome {
    Approved {
        /// The exec-turn prompt: the approved plan text followed by the
        /// original task, per the "Follow this approved plan:" template.
        effective_task: String,
    },
    Rejected {
        outcome: kode_agent::AgentOutcome,
    },
}

/// Runs the plan-mode turn: a model call under an empty tool registry (no
/// tools disabled means none are ever offered) that streams a numbered plan
/// for `task` into the transcript via the normal `ModelToken` event path,
/// then asks `handler` to approve or reject it via the same
/// `PermissionHandler::confirm` mechanism used for mutating tool calls.
/// Reuses the same compiled `context`/`history` the exec turn uses — see the
/// design note in `run_task`. Factored out of `run_task` (rather than the
/// model-provider-selection glue around it) so it's directly unit-testable
/// with `kode_model::MockModel`.
#[allow(clippy::too_many_arguments)]
async fn run_plan_phase(
    model: Arc<dyn kode_model::Model>,
    events: &EventBus,
    handler: Arc<dyn PermissionHandler>,
    agent_cfg: &AgentConfig,
    effort: Option<String>,
    task: &str,
    context: Option<&str>,
    history: &[kode_agent::HistoryTurn],
    history_truncated: bool,
    ctx: &ToolContext,
) -> anyhow::Result<PlanOutcome> {
    // Empty registry — no tools are ever offered on this turn, so the
    // permission mode is moot; `Deny` names that intent explicitly.
    let tools = ToolRuntime::new(ToolRegistry::new(), PermissionMode::Deny, handler.clone());
    let plan_agent = Agent::new(model, tools, events.clone(), agent_cfg).with_effort(effort);

    let plan_prompt = format!("{task}\n\n{PLAN_INSTRUCTION}");
    let outcome = plan_agent
        .run_with_context(&plan_prompt, context, history, history_truncated, ctx)
        .await
        .map_err(|err| anyhow::anyhow!(err))?;

    let approved = handler.confirm("execute this plan?").await;
    if approved {
        let plan_text = outcome.final_text.trim().to_string();
        let effective_task = format!("Follow this approved plan:\n{plan_text}\n\nTask: {task}");
        Ok(PlanOutcome::Approved { effective_task })
    } else {
        Ok(PlanOutcome::Rejected { outcome })
    }
}

/// Whether the Ingat autostart flow should run: only when the config opts
/// in and this process hasn't already made its one attempt. Pure — kept
/// separate from the I/O so the decision is unit-testable on its own.
fn should_attempt_autostart(autostart: bool, already_attempted: bool) -> bool {
    autostart && !already_attempted
}

/// Polls `adapter.health()` up to `attempts` times (2s timeout per call),
/// sleeping `interval` between attempts, returning `true` on the first
/// success and `false` if every attempt fails. With the defaults used below
/// (10 attempts, 500ms interval) this budgets roughly 5s total.
async fn poll_ingat_health(adapter: &IngatAdapter, attempts: u32, interval: Duration) -> bool {
    for attempt in 0..attempts {
        if let Ok(Ok(())) = tokio::time::timeout(Duration::from_secs(2), adapter.health()).await {
            return true;
        }
        if attempt + 1 < attempts {
            tokio::time::sleep(interval).await;
        }
    }
    false
}

/// Called only after the initial Ingat health check has already failed.
/// When autostart is enabled and this process hasn't tried yet, locates the
/// installed service, starts it detached, and re-polls health — reporting
/// each step via `KodeEvent::SourcedNote`. Returns `Some` when the retry
/// succeeds, `None` otherwise (task proceeds without engineering memory).
async fn maybe_autostart_ingat(
    cfg: &IngatConfig,
    events: &EventBus,
) -> Option<Arc<dyn EngineeringMemory>> {
    let already_attempted = INGAT_AUTOSTART_ATTEMPTED.get().is_some();
    if !should_attempt_autostart(cfg.autostart, already_attempted) {
        return None;
    }
    // Mark the attempt immediately (not just on success) — this is a
    // one-shot-per-process flag regardless of outcome.
    let _ = INGAT_AUTOSTART_ATTEMPTED.set(());

    events.emit(KodeEvent::SourcedNote {
        text: "ingat: service not running — starting it".to_string(),
        source: NoteSource::Ingat,
    });

    let Some(path) = crate::setup::find_mcp_service() else {
        events.emit(KodeEvent::SourcedNote {
            text: "ingat: service not installed — run kode setup".to_string(),
            source: NoteSource::Ingat,
        });
        return None;
    };

    if let Err(e) = crate::setup::spawn_detached(&path) {
        events.emit(KodeEvent::SourcedNote {
            text: format!("ingat: failed to start service: {e}"),
            source: NoteSource::Ingat,
        });
        return None;
    }

    let adapter = IngatAdapter::new(cfg);
    if poll_ingat_health(&adapter, 10, Duration::from_millis(500)).await {
        events.emit(KodeEvent::SourcedNote {
            text: "ingat: service started".to_string(),
            source: NoteSource::Ingat,
        });
        Some(Arc::new(adapter) as Arc<dyn EngineeringMemory>)
    } else {
        events.emit(KodeEvent::SourcedNote {
            text: "ingat: service did not become healthy — continuing without memory".to_string(),
            source: NoteSource::Ingat,
        });
        None
    }
}

/// Verdict of a verification pass, derived from whether it found zero
/// failures (`ok`) and whether any check actually ran (`ran_any`).
/// `NoChecks` short-circuits retry — retrying can't conjure checks into
/// existence — while still counting as "nothing failed" for exit purposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    Verified,
    Failed,
    NoChecks,
}

fn verification_verdict(ok: bool, ran_any: bool) -> Verdict {
    if !ran_any {
        Verdict::NoChecks
    } else if ok {
        Verdict::Verified
    } else {
        Verdict::Failed
    }
}

fn verification_status(verdict: Verdict) -> VerificationStatus {
    match verdict {
        Verdict::Verified => VerificationStatus::Verified,
        Verdict::Failed => VerificationStatus::Failed,
        Verdict::NoChecks => VerificationStatus::NoChecks,
    }
}

/// Aggregates two agent runs (initial + retry) into the metrics
/// `TaskFinished` reports: summed iterations, summed tool calls, summed
/// input/output tokens, and `mutated` OR'd across both runs (a mutation in
/// either run means the workspace changed).
fn combine_outcomes(
    a: &kode_agent::AgentOutcome,
    b: &kode_agent::AgentOutcome,
) -> (u32, u32, u64, u64, bool) {
    (
        a.iterations + b.iterations,
        a.tool_calls + b.tool_calls,
        a.usage.input_tokens + b.usage.input_tokens,
        a.usage.output_tokens + b.usage.output_tokens,
        a.mutated || b.mutated,
    )
}

/// Builds a `KodeEvent::Knowledge` digest from a compiled context. Pure —
/// no I/O, safe to unit test with a hand-built `CompiledContext`.
fn knowledge_from(compiled: &CompiledContext, budget: usize) -> KodeEvent {
    KodeEvent::Knowledge {
        zindeks: zindeks_lines(compiled),
        ingat: ingat_lines(compiled),
        git: git_lines(compiled),
        context_tokens: compiled.stats.compiled_tokens,
        budget_tokens: budget,
    }
}

/// Up to 3 distinct `**path**`-style file headers pulled from the
/// CodeIntelligence section body(ies), rendered as `path (score)` when a
/// trailing `(...)` is present, else just `path`. Falls back to a section
/// count/token summary when no such headers parse.
fn zindeks_lines(compiled: &CompiledContext) -> Vec<String> {
    let intel_sections: Vec<&kode_context::ContextSection> = compiled
        .sections
        .iter()
        .filter(|s| s.source == ContextSource::CodeIntelligence)
        .collect();
    if intel_sections.is_empty() {
        return Vec::new();
    }

    let mut lines: Vec<String> = Vec::new();
    for section in &intel_sections {
        for raw in section.body.lines() {
            if let Some(parsed) = parse_zindeks_header(raw)
                && !lines.contains(&parsed)
            {
                lines.push(parsed);
                if lines.len() == 3 {
                    return lines;
                }
            }
        }
    }

    if lines.is_empty() {
        let tokens: usize = intel_sections.iter().map(|s| s.tokens).sum();
        vec![format!(
            "{} context sections · {tokens} tokens",
            intel_sections.len()
        )]
    } else {
        lines
    }
}

/// Parses a markdown file-header line like `**src/foo.rs** (0.83)` into
/// `"src/foo.rs (0.83)"`, or `**src/foo.rs**` into `"src/foo.rs"`. Returns
/// `None` when `line` isn't a `**...**`-style header.
fn parse_zindeks_header(line: &str) -> Option<String> {
    let trimmed = line.trim();
    let rest = trimmed.strip_prefix("**")?;
    let (path, after) = rest.split_once("**")?;
    let path = path.trim();
    if path.is_empty() {
        return None;
    }
    let after = after.trim();
    if after.starts_with('(') && after.ends_with(')') {
        Some(format!("{path} {after}"))
    } else {
        Some(path.to_string())
    }
}

/// First bullet's summary text (leading `- **[kind]** ` stripped) per
/// Memory-source section, max 2, each truncated to 60 chars (char-safe).
/// When the bullet carries a trailing `(0.NN)` confidence tag (see
/// `kode_context::compile::format_memory_bullet`), it's split off before
/// truncation and re-appended as a ` ┄ 0.NN` suffix — the TUI knowledge
/// band renders that suffix dim, distinct from the ingat text color.
fn ingat_lines(compiled: &CompiledContext) -> Vec<String> {
    let mut lines = Vec::new();
    for section in compiled
        .sections
        .iter()
        .filter(|s| s.source == ContextSource::Memory)
    {
        if let Some(first_bullet) = section.body.lines().next() {
            let (text, confidence) = split_confidence_suffix(strip_bullet_prefix(first_bullet));
            let mut line = truncate_chars(text, 60);
            if let Some(score) = confidence {
                line.push_str(&format!(" \u{2504} {score:.2}"));
            }
            lines.push(line);
            if lines.len() == 2 {
                break;
            }
        }
    }
    lines
}

/// Splits a trailing `" (0.87)"`-style confidence tag off `text`, returning
/// `(text_without_tag, Some(score))` when the tag parses as a float, else
/// `(text, None)`. The tag is the *last* parenthesized group only — earlier
/// parens (e.g. the `_(inferred, low confidence)_` marker) are left alone
/// since they don't sit at the very end of the string.
fn split_confidence_suffix(text: &str) -> (&str, Option<f32>) {
    let trimmed = text.trim_end();
    if let Some(rest) = trimmed.strip_suffix(')')
        && let Some(open) = rest.rfind('(')
        && let Ok(score) = rest[open + 1..].parse::<f32>()
    {
        return (trimmed[..open].trim_end(), Some(score));
    }
    (text, None)
}

/// Strips the `- **[kind]** ` prefix `format_memory_bullet` in
/// `kode-context` adds ahead of a memory's summary text.
fn strip_bullet_prefix(bullet: &str) -> &str {
    let trimmed = bullet.trim_start();
    let Some(after_dash) = trimmed.strip_prefix("- ") else {
        return trimmed;
    };
    let Some(after_open) = after_dash.strip_prefix("**[") else {
        return after_dash;
    };
    match after_open.split_once("]** ") {
        Some((_, rest)) => rest,
        None => after_open,
    }
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut truncated: String = s.chars().take(max).collect();
        truncated.push('…');
        truncated
    }
}

/// `"{n} files changed"` from the Git section's `status:` block, or an
/// empty vec when there is no Git section (clean tree / not a repo).
fn git_lines(compiled: &CompiledContext) -> Vec<String> {
    let Some(section) = compiled
        .sections
        .iter()
        .find(|s| s.source == ContextSource::Git)
    else {
        return Vec::new();
    };
    let after_status = section
        .body
        .strip_prefix("status:\n")
        .unwrap_or(&section.body);
    let status_block = after_status
        .split("\n\ndiff:\n")
        .next()
        .unwrap_or(after_status);
    let n = status_block
        .lines()
        .filter(|l| !l.trim().is_empty())
        .count();
    if n == 0 {
        Vec::new()
    } else {
        vec![format!("{n} files changed")]
    }
}

/// Emits one `VerifyStep` event per `StepResult` — the per-step provenance
/// the TUI's `V` gutter and Ledger view render. Replaces the old
/// `Note`-per-step reporting so a step is only reported once.
fn emit_verify_steps(events: &EventBus, report: &kode_verify::VerificationReport) {
    for step in &report.steps {
        let (passed, skipped) = match &step.status {
            kode_verify::StepStatus::Passed => (true, false),
            kode_verify::StepStatus::Failed => (false, false),
            kode_verify::StepStatus::Skipped(_) => (false, true),
        };
        events.emit(KodeEvent::VerifyStep {
            name: step.name.clone(),
            passed,
            skipped,
            duration_ms: step.duration.as_millis() as u64,
        });
    }
}

#[cfg(test)]
mod plan_phase_tests {
    use super::*;
    use kode_model::{FinishReason, Message, MockModel, StreamEvent};
    use kode_tools::permission::{AutoApprove, AutoDeny};

    fn temp_dir(label: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "kode-pipeline-plan-test-{label}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn ctx(root: std::path::PathBuf) -> ToolContext {
        ToolContext {
            workspace_root: root,
            cancel: CancellationToken::new(),
        }
    }

    fn plan_script(plan_text: &str) -> Vec<StreamEvent> {
        vec![
            StreamEvent::TextDelta(plan_text.to_string()),
            StreamEvent::Finished {
                reason: FinishReason::Stop,
                usage: None,
            },
        ]
    }

    #[tokio::test]
    async fn approved_plan_is_injected_into_effective_task() {
        let dir = temp_dir("approve");
        let mock = Arc::new(MockModel::new());
        mock.push_script(plan_script("1. do the thing\n2. verify it"));

        let outcome = run_plan_phase(
            mock.clone(),
            &EventBus::new(64),
            Arc::new(AutoApprove),
            &AgentConfig::default(),
            None,
            "add a widget",
            None,
            &[],
            false,
            &ctx(dir),
        )
        .await
        .unwrap();

        match outcome {
            PlanOutcome::Approved { effective_task } => {
                assert!(effective_task.starts_with("Follow this approved plan:\n"));
                assert!(effective_task.contains("1. do the thing\n2. verify it"));
                assert!(effective_task.ends_with("\n\nTask: add a widget"));
            }
            PlanOutcome::Rejected { .. } => panic!("expected Approved"),
        }

        // The model saw the plan-mode prompt (task + PLAN_INSTRUCTION) with
        // no tools offered — not the raw task and not the builtin registry.
        let requests = mock.requests();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].tools.is_empty());
        let saw_plan_prompt = requests[0].messages.iter().any(|m| {
            matches!(m, Message::User(t) if t == &format!("add a widget\n\n{PLAN_INSTRUCTION}"))
        });
        assert!(saw_plan_prompt, "model did not see the plan-mode prompt");
    }

    #[tokio::test]
    async fn rejected_plan_never_triggers_a_second_model_call() {
        let dir = temp_dir("reject");
        let mock = Arc::new(MockModel::new());
        mock.push_script(plan_script("1. do the thing"));

        let outcome = run_plan_phase(
            mock.clone(),
            &EventBus::new(64),
            Arc::new(AutoDeny),
            &AgentConfig::default(),
            None,
            "add a widget",
            None,
            &[],
            false,
            &ctx(dir),
        )
        .await
        .unwrap();

        assert!(matches!(outcome, PlanOutcome::Rejected { .. }));
        // Only the plan turn's one request happened — MockModel only had one
        // script queued, so a second (exec-turn) call would have errored
        // "mock: no script" instead of `run_plan_phase` returning Ok. This
        // mirrors `run_task`'s real control flow: the exec-turn
        // `agent.run_with_context` call only runs in the `Approved` arm.
        assert_eq!(mock.requests().len(), 1);
    }
}

#[cfg(test)]
mod ingat_autostart_tests {
    use super::should_attempt_autostart;

    #[test]
    fn attempts_when_enabled_and_not_yet_attempted() {
        assert!(should_attempt_autostart(true, false));
    }

    #[test]
    fn skips_when_disabled() {
        assert!(!should_attempt_autostart(false, false));
    }

    #[test]
    fn skips_when_already_attempted() {
        assert!(!should_attempt_autostart(true, true));
    }

    #[test]
    fn skips_when_disabled_and_already_attempted() {
        assert!(!should_attempt_autostart(false, true));
    }
}

#[cfg(test)]
mod verification_tests {
    use super::*;
    use kode_agent::AgentOutcome;
    use kode_model::Usage;

    fn outcome(
        iterations: u32,
        tool_calls: u32,
        input: u64,
        output: u64,
        mutated: bool,
    ) -> AgentOutcome {
        AgentOutcome {
            final_text: String::new(),
            iterations,
            tool_calls,
            usage: Usage {
                input_tokens: input,
                output_tokens: output,
            },
            mutated,
        }
    }

    #[test]
    fn verdict_no_checks_when_nothing_ran() {
        assert_eq!(verification_verdict(true, false), Verdict::NoChecks);
        // Even a report claiming `ok: false` with nothing run is NoChecks —
        // `ran_any` gates first.
        assert_eq!(verification_verdict(false, false), Verdict::NoChecks);
    }

    #[test]
    fn verdict_verified_when_ok_and_ran() {
        assert_eq!(verification_verdict(true, true), Verdict::Verified);
    }

    #[test]
    fn verdict_failed_when_not_ok_and_ran() {
        assert_eq!(verification_verdict(false, true), Verdict::Failed);
    }

    #[test]
    fn combine_outcomes_sums_metrics_and_ors_mutated() {
        let a = outcome(3, 5, 100, 200, true);
        let b = outcome(2, 4, 50, 75, false);

        let (iterations, tool_calls, input_tokens, output_tokens, mutated) =
            combine_outcomes(&a, &b);

        assert_eq!(iterations, 5);
        assert_eq!(tool_calls, 9);
        assert_eq!(input_tokens, 150);
        assert_eq!(output_tokens, 275);
        assert!(mutated);
    }

    #[test]
    fn combine_outcomes_mutated_false_when_neither_mutated() {
        let a = outcome(1, 1, 10, 10, false);
        let b = outcome(1, 1, 10, 10, false);

        let (.., mutated) = combine_outcomes(&a, &b);
        assert!(!mutated);
    }
}

#[cfg(test)]
mod knowledge_tests {
    use super::*;
    use kode_context::{ContextSection, ContextStats};

    fn section(source: ContextSource, title: &str, body: &str) -> ContextSection {
        ContextSection {
            source,
            title: title.to_string(),
            body: body.to_string(),
            tokens: body.len().div_ceil(4),
        }
    }

    fn compiled(sections: Vec<ContextSection>, compiled_tokens: usize) -> CompiledContext {
        CompiledContext {
            sections,
            stats: ContextStats {
                compiled_tokens,
                ..Default::default()
            },
        }
    }

    #[test]
    fn knowledge_from_empty_compiled_yields_all_empty_vecs() {
        let c = compiled(vec![], 0);
        let ev = knowledge_from(&c, 16_000);
        match ev {
            KodeEvent::Knowledge {
                zindeks,
                ingat,
                git,
                context_tokens,
                budget_tokens,
            } => {
                assert!(zindeks.is_empty());
                assert!(ingat.is_empty());
                assert!(git.is_empty());
                assert_eq!(context_tokens, 0);
                assert_eq!(budget_tokens, 16_000);
            }
            other => panic!("expected Knowledge, got {other:?}"),
        }
    }

    #[test]
    fn knowledge_from_full_sections_extracts_all_three_sources() {
        let intel_body = "**src/foo.rs** (0.91)\nsome context\n**src/bar.rs** (0.80)\n**src/baz.rs**\n**src/qux.rs**\n";
        let memory_body = "- **[project-rule]** always prefix shell commands with rtk immediately every single time no exceptions — full body text here";
        let git_body = "status:\nM foo.rs\nA bar.rs\n\ndiff:\n+ line\n- line";

        let c = compiled(
            vec![
                section(
                    ContextSource::CodeIntelligence,
                    "Repository context",
                    intel_body,
                ),
                section(
                    ContextSource::Memory,
                    "Project rules & conventions",
                    memory_body,
                ),
                section(ContextSource::Git, "Uncommitted changes", git_body),
            ],
            4200,
        );

        let ev = knowledge_from(&c, 16_000);
        match ev {
            KodeEvent::Knowledge {
                zindeks,
                ingat,
                git,
                context_tokens,
                budget_tokens,
            } => {
                assert_eq!(
                    zindeks,
                    vec![
                        "src/foo.rs (0.91)".to_string(),
                        "src/bar.rs (0.80)".to_string(),
                        "src/baz.rs".to_string(),
                    ]
                );
                assert_eq!(
                    ingat,
                    vec![
                        "always prefix shell commands with rtk immediately every sing…".to_string()
                    ]
                );
                assert_eq!(git, vec!["2 files changed".to_string()]);
                assert_eq!(context_tokens, 4200);
                assert_eq!(budget_tokens, 16_000);
            }
            other => panic!("expected Knowledge, got {other:?}"),
        }
    }

    #[test]
    fn knowledge_from_unparseable_zindeks_markdown_falls_back_to_summary() {
        let intel_body = "no bold headers here\njust plain repository context text";
        let c = compiled(
            vec![section(
                ContextSource::CodeIntelligence,
                "Repository context",
                intel_body,
            )],
            10,
        );

        let ev = knowledge_from(&c, 16_000);
        match ev {
            KodeEvent::Knowledge { zindeks, .. } => {
                assert_eq!(zindeks.len(), 1);
                assert!(zindeks[0].contains("context sections"));
                assert!(zindeks[0].contains("tokens"));
            }
            other => panic!("expected Knowledge, got {other:?}"),
        }
    }

    #[test]
    fn split_confidence_suffix_extracts_trailing_score() {
        assert_eq!(
            split_confidence_suffix("always prefix with rtk (0.87)"),
            ("always prefix with rtk", Some(0.87))
        );
    }

    #[test]
    fn split_confidence_suffix_leaves_earlier_parens_alone_without_trailing_score() {
        let text = "rule text _(inferred, low confidence)_";
        assert_eq!(split_confidence_suffix(text), (text, None));
    }

    #[test]
    fn split_confidence_suffix_none_when_no_trailing_parens() {
        assert_eq!(
            split_confidence_suffix("plain text, no tag"),
            ("plain text, no tag", None)
        );
    }

    #[test]
    fn split_confidence_suffix_handles_inferred_marker_before_score() {
        let text = "rule text _(inferred, low confidence)_ (0.42)";
        assert_eq!(
            split_confidence_suffix(text),
            ("rule text _(inferred, low confidence)_", Some(0.42))
        );
    }

    #[test]
    fn ingat_lines_appends_dim_confidence_suffix_when_tag_present() {
        let c = compiled(
            vec![section(
                ContextSource::Memory,
                "Project rules & conventions",
                "- **[project-rule]** always prefix with rtk (0.87)",
            )],
            10,
        );
        assert_eq!(
            ingat_lines(&c),
            vec!["always prefix with rtk \u{2504} 0.87".to_string()]
        );
    }

    #[test]
    fn ingat_lines_omits_suffix_when_no_tag_present() {
        let c = compiled(
            vec![section(
                ContextSource::Memory,
                "Project rules & conventions",
                "- **[project-rule]** always prefix with rtk",
            )],
            10,
        );
        assert_eq!(ingat_lines(&c), vec!["always prefix with rtk".to_string()]);
    }

    #[test]
    fn git_lines_empty_when_no_git_section() {
        let c = compiled(
            vec![section(
                ContextSource::CodeIntelligence,
                "Repository context",
                "**src/foo.rs**",
            )],
            10,
        );
        assert!(git_lines(&c).is_empty());
    }
}
