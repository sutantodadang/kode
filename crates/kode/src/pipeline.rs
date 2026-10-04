use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use kode_agent::{Agent, SubagentTool};
use kode_context::{CompiledContext, ContextCompiler, ContextRequest, ContextSource};
use kode_core::config::{AgentConfig, KodeConfig, PermissionMode};
use kode_core::event::{EventBus, KodeEvent, NoteSource, TaskStep};
use kode_core::{CancellationToken, UserInput};
use kode_intel::CodeIntelligence;
use kode_memory::{EngineeringMemory, MemorySearchTool, RememberTool};
use kode_model::{OpenAiModel, OpenAiOptions, Usage};
use kode_tools::ToolContext;
use kode_tools::permission::PermissionHandler;
use kode_tools::registry::{ToolRegistry, ToolRuntime};
use kode_tools::skills::SkillCatalog;
use kode_tools::tools::UseSkill;
use tokio::sync::mpsc;

use crate::intel_tools::{CodeSearchTool, FileOutlineTool};
use crate::session_runtime::SessionRuntime;

/// Appended to the task text for the plan-mode turn (see
/// [`run_plan_phase`]). The turn exposes no implementation tools; only the
/// read-only `use_skill` tool may be registered.
const PLAN_INSTRUCTION: &str = "Before making any changes, write a concise numbered plan for \
accomplishing this task: the concrete steps you would take and which files you would touch. \
Do not write code. You have no implementation tools available for this turn; `use_skill` may be available for reading relevant instructions. Describe the plan, then stop.";

static REPORTED_CONTEXT_BUDGETS: OnceLock<Mutex<HashSet<(String, String, u32)>>> = OnceLock::new();

fn format_token_count(tokens: u32) -> String {
    if tokens < 1_000 {
        tokens.to_string()
    } else if tokens < 1_000_000 {
        format!("{:.0}k", tokens as f64 / 1_000.0)
    } else {
        format!("{:.1}m", tokens as f64 / 1_000_000.0)
    }
}

/// One cache key per Kode process. Providers use it to route every request
/// of this process to the same warm prompt cache.
pub(crate) fn new_cache_key() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    cache_key_at(nanos)
}

static CACHE_KEY_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The sequence number keeps keys distinct when two calls fall in the same
/// clock tick; some platforms only report microseconds.
fn cache_key_at(nanos: u128) -> String {
    let seq = CACHE_KEY_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("kode-{}-{nanos}-{seq}", std::process::id())
}

fn should_report_context_budget(provider: &str, model: &str, window: u32) -> bool {
    REPORTED_CONTEXT_BUDGETS
        .get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .map(|mut reported| reported.insert((provider.to_string(), model.to_string(), window)))
        .unwrap_or(false)
}

fn register_code_intelligence_tools(
    registry: &mut ToolRegistry,
    intel: &Option<Arc<dyn CodeIntelligence>>,
) {
    if let Some(code_intel) = intel {
        registry.register(Arc::new(CodeSearchTool::new(code_intel.clone())));
        registry.register(Arc::new(FileOutlineTool::new(code_intel.clone())));
    }
}

fn register_memory_tools(
    registry: &mut ToolRegistry,
    memory: &Option<Arc<dyn EngineeringMemory>>,
    repository: Option<String>,
) {
    if let Some(memory) = memory {
        registry.register(Arc::new(MemorySearchTool::new(
            memory.clone(),
            repository.clone(),
        )));
        registry.register(Arc::new(RememberTool::new(memory.clone(), repository)));
    }
}

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

struct VerificationService<'a> {
    root: &'a Path,
    config: &'a kode_core::config::VerifyConfig,
    cancel: &'a CancellationToken,
}

impl VerificationService<'_> {
    async fn run(&self) -> kode_verify::VerificationReport {
        let profile = kode_verify::detect_with_config(self.root, self.config);
        kode_verify::run_verification(self.root, &profile, self.cancel).await
    }
}

async fn run_verification_phase(
    service: &VerificationService<'_>,
    events: &EventBus,
) -> (kode_verify::VerificationReport, Verdict) {
    events.emit(KodeEvent::VerificationStarted);
    let report = service.run().await;
    emit_verify_steps(events, &report);
    let verdict = verification_verdict(report.ok, report.ran_any());
    events.emit(KodeEvent::VerificationFinished {
        ok: verdict == Verdict::Verified,
    });
    events.emit(KodeEvent::TaskProgress {
        step: TaskStep::Verify,
        done: verdict == Verdict::Verified,
    });
    if verdict == Verdict::NoChecks {
        events.emit(KodeEvent::Note {
            text: "no verification checks for this project — changes are unverified".to_string(),
        });
    }
    (report, verdict)
}

pub(crate) struct ModelFactory;

impl ModelFactory {
    pub(crate) fn create(config: &KodeConfig) -> anyhow::Result<Arc<dyn kode_model::Model>> {
        if config.model.model.is_empty() {
            anyhow::bail!("set model.model in .kode/config.toml");
        }
        Self::build_model(&config.model.provider, &config.model.model)
    }

    /// Builds a model handle for one provider/model pair. Shared by the root
    /// session model and `[agent.subagents.models.*]` delegation tiers.
    fn build_model(provider: &str, model: &str) -> anyhow::Result<Arc<dyn kode_model::Model>> {
        if model.is_empty() {
            anyhow::bail!("model id must not be empty");
        }
        let model: Arc<dyn kode_model::Model> = match provider {
            "openai" => {
                let api_key = std::env::var("OPENAI_API_KEY")
                    .or_else(|_| std::env::var("KODE_API_KEY"))
                    .map_err(|_| anyhow::anyhow!("set OPENAI_API_KEY to run `kode exec`"))?;

                let mut opts = OpenAiOptions {
                    api_key,
                    model: model.to_string(),
                    ..Default::default()
                };
                if let Ok(base_url) = std::env::var("OPENAI_BASE_URL") {
                    opts.base_url = base_url;
                }
                Arc::new(OpenAiModel::new(opts))
            }
            "codex" => {
                let auth_path = kode_model::codex::default_auth_path().ok_or_else(|| {
                    anyhow::anyhow!("cannot resolve home directory for codex auth")
                })?;
                let auth =
                    kode_model::codex::load(&auth_path).map_err(|e| anyhow::anyhow!("{e}"))?;

                if auth.auth_mode == "apikey" {
                    let api_key = auth.api_key.clone().ok_or_else(|| {
                        anyhow::anyhow!(
                            "codex auth.json has auth_mode=apikey but no OPENAI_API_KEY — run: kode auth login codex"
                        )
                    })?;
                    Arc::new(OpenAiModel::new(OpenAiOptions {
                        api_key,
                        model: model.to_string(),
                        ..Default::default()
                    }))
                } else {
                    Arc::new(
                        kode_model::CodexModel::new(auth_path, model.to_string())
                            .map_err(|e| anyhow::anyhow!("{e}"))?,
                    )
                }
            }
            "opencode-go" | "opencode" | "kilo" | "lmstudio" => {
                let auth_path = kode_model::opencode::default_auth_path().ok_or_else(|| {
                    anyhow::anyhow!("cannot resolve home directory for opencode auth")
                })?;
                Arc::new(
                    kode_model::opencode::resolve(provider, model.to_string(), &auth_path, None)
                        .map_err(|e| anyhow::anyhow!("{e}"))?,
                )
            }
            "anthropic" => {
                let auth_path = kode_model::anthropic::default_auth_path().ok_or_else(|| {
                    anyhow::anyhow!("cannot resolve home directory for anthropic auth")
                })?;
                Arc::new(
                    kode_model::AnthropicModel::new(auth_path, model.to_string())
                        .map_err(|e| anyhow::anyhow!("{e}"))?,
                )
            }
            "antigravity" => {
                let auth_path = kode_model::antigravity::default_auth_path().ok_or_else(|| {
                    anyhow::anyhow!("cannot resolve home directory for antigravity auth")
                })?;
                Arc::new(
                    kode_model::AntigravityModel::new(auth_path, model.to_string())
                        .map_err(|e| anyhow::anyhow!("{e}"))?,
                )
            }
            other => anyhow::bail!(
                "provider {other} not supported yet (supported: openai, anthropic, antigravity, codex, opencode-go, opencode, kilo, lmstudio)"
            ),
        };
        Ok(model)
    }
}

/// Runs one agentic task end-to-end: model/config setup, code intelligence
/// and engineering memory binding, context compilation, the agent loop, and
/// post-edit verification with a single retry. This is the single code path
/// shared by `kode exec` (headless) and the TUI — it communicates *only*
/// through `events`, never via stdout/stderr directly.
#[allow(clippy::too_many_arguments)]
#[allow(dead_code)]
pub async fn run_task(
    task: &str,
    cwd: &Path,
    config: &KodeConfig,
    events: EventBus,
    handler: Arc<dyn PermissionHandler>,
    cancel: CancellationToken,
    history: &[kode_agent::HistoryTurn],
    plan_mode: bool,
    steering: Option<mpsc::UnboundedReceiver<UserInput>>,
) -> anyhow::Result<TaskOutcome> {
    let runtime = SessionRuntime::new();
    run_task_with_input(
        &UserInput::text(task),
        cwd,
        config,
        events,
        handler,
        cancel,
        history,
        plan_mode,
        steering,
        None,
        &runtime,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn run_task_with_input(
    input: &UserInput,
    cwd: &Path,
    config: &KodeConfig,
    events: EventBus,
    handler: Arc<dyn PermissionHandler>,
    cancel: CancellationToken,
    history: &[kode_agent::HistoryTurn],
    plan_mode: bool,
    steering: Option<mpsc::UnboundedReceiver<UserInput>>,
    cache_key: Option<String>,
    runtime: &SessionRuntime,
) -> anyhow::Result<TaskOutcome> {
    let routed = crate::routing::route_task(input, cwd, config, plan_mode, &events, &cancel).await;
    let result = execute_task(
        input,
        cwd,
        &routed.applied.config,
        events.clone(),
        handler,
        cancel,
        history,
        routed.applied.plan_mode,
        steering,
        config,
        routed.reranker,
        cache_key,
        runtime,
    )
    .await;
    if let Some(decision) = &routed.decision
        && let Some(text) = crate::routing::log_route(
            cwd,
            &config.router,
            &input.text,
            decision,
            crate::routing::outcome_of(&result),
        )
    {
        events.emit(KodeEvent::Note { text });
    }
    if config.router.training.enabled
        && let (Some(decision), Some(route_input), Ok(outcome)) =
            (&routed.decision, &routed.route_input, &result)
    {
        let applied = &routed.applied.config;
        let note = match ModelFactory::create(applied) {
            Ok(model) => {
                let files_changed = kode_context::git::repo_state(cwd)
                    .await
                    .map(|s| s.numstat.len())
                    .unwrap_or(0);
                crate::training::capture(
                    model.as_ref(),
                    crate::training::CaptureInput {
                        root: cwd,
                        model_id: format!("{}/{}", applied.model.provider, applied.model.model),
                        author: crate::training::git_user_name(cwd).await,
                        route_input,
                        decision,
                        outcome,
                        files_changed,
                    },
                )
                .await
            }
            Err(e) => Some(format!(
                "router: label skipped (teacher model unavailable: {e})"
            )),
        };
        if let Some(text) = note {
            events.emit(KodeEvent::Note { text });
        }
    }
    result
}

#[allow(clippy::too_many_arguments)]
async fn execute_task(
    input: &UserInput,
    cwd: &Path,
    config: &KodeConfig,
    events: EventBus,
    handler: Arc<dyn PermissionHandler>,
    cancel: CancellationToken,
    history: &[kode_agent::HistoryTurn],
    plan_mode: bool,
    mut steering: Option<mpsc::UnboundedReceiver<UserInput>>,
    fallback_config: &KodeConfig,
    reranker: Option<Arc<dyn kode_context::ContextReranker>>,
    cache_key: Option<String>,
    runtime: &SessionRuntime,
) -> anyhow::Result<TaskOutcome> {
    let model = match ModelFactory::create(config) {
        Ok(model) => model,
        Err(error) if config.model != fallback_config.model => {
            events.emit(KodeEvent::Note {
                text: format!("router-selected model unavailable: {error} — using the root model"),
            });
            ModelFactory::create(fallback_config)?
        }
        Err(error) => return Err(error),
    };
    let detected_context_window = if config.agent.max_context_tokens == 0 {
        kode_model::catalog::context_window_tokens(&config.model.provider, &config.model.model)
            .await
    } else {
        None
    };
    let agent_config = config.agent.resolved(detected_context_window);
    if (config.agent.max_context_tokens == 0
        || config.agent.context_budget_tokens == 0
        || config.agent.history_budget_tokens == 0)
        && should_report_context_budget(
            &config.model.provider,
            &config.model.model,
            agent_config.max_context_tokens,
        )
    {
        events.emit(KodeEvent::Note {
            text: format!(
                "adaptive context: {} window · {} repository · {} recent history · auto-compact {}",
                format_token_count(agent_config.max_context_tokens),
                format_token_count(agent_config.context_budget_tokens),
                format_token_count(agent_config.history_budget_tokens),
                if agent_config.auto_compact {
                    "on"
                } else {
                    "off"
                }
            ),
        });
    }

    let ctx = ToolContext {
        workspace_root: cwd.to_path_buf(),
        cancel,
    };

    // Keep a handle on the bound backend so we can call `ensure_bound` again
    // (as an incremental refresh) after edits. The trait exposes lifecycle
    // parity, so no concrete adapter type is retained here.
    let mut bound_backend: Option<Arc<dyn CodeIntelligence>> = None;
    let intel: Option<Arc<dyn CodeIntelligence>> = match runtime.intel(&config.zindeks, cwd).await {
        Ok(Some(handle)) => {
            // A reused handle is already bound. Without the watcher it
            // still needs a refresh to see edits made between tasks.
            let bind = if handle.fresh || !handle.backend.watching() {
                handle.backend.ensure_bound().await
            } else {
                Ok(())
            };
            match bind {
                Ok(()) => {
                    bound_backend = Some(handle.backend.clone());
                    Some(handle.backend)
                }
                Err(e) => {
                    // Do not keep a backend that could not bind: the
                    // next task must try again, e.g. after `kode index`.
                    runtime.forget_intel().await;
                    events.emit(KodeEvent::SourcedNote {
                        text: format!("code intelligence unavailable: {e}"),
                        source: NoteSource::Zindeks,
                    });
                    None
                }
            }
        }
        Ok(None) => None,
        Err(e) => {
            events.emit(KodeEvent::SourcedNote {
                text: format!("code intelligence unavailable: {e}"),
                source: NoteSource::Zindeks,
            });
            None
        }
    };

    let memory: Option<Arc<dyn EngineeringMemory>> = match runtime.memory(&config.ingat).await {
        Ok(Some(backend)) => {
            match tokio::time::timeout(Duration::from_secs(3), backend.health()).await {
                Ok(Ok(())) => Some(backend),
                Ok(Err(e)) => {
                    runtime.forget_memory().await;
                    events.emit(KodeEvent::SourcedNote {
                        text: format!("engineering memory unavailable: {e}"),
                        source: NoteSource::Ingat,
                    });
                    None
                }
                Err(_) => {
                    runtime.forget_memory().await;
                    events.emit(KodeEvent::SourcedNote {
                        text: "engineering memory unavailable: request timed out".to_string(),
                        source: NoteSource::Ingat,
                    });
                    None
                }
            }
        }
        Ok(None) => None,
        Err(e) => {
            events.emit(KodeEvent::SourcedNote {
                text: format!("engineering memory unavailable: {e}"),
                source: NoteSource::Ingat,
            });
            None
        }
    };

    let skills = Arc::new(SkillCatalog::discover(cwd));
    if !skills.is_empty() {
        events.emit(KodeEvent::Note {
            text: format!("{} skills available", skills.len()),
        });
    }

    // Part of the cached system prefix: identical for every task while the
    // skill set is unchanged.
    let skill_summary = skills.prompt_summary();

    let effort = if config.model.effort.is_empty() {
        None
    } else {
        Some(config.model.effort.clone())
    };
    let repository = cwd
        .file_name()
        .map(|name| name.to_string_lossy().to_string());

    let mut registry = ToolRegistry::with_builtins();
    register_code_intelligence_tools(&mut registry, &intel);
    if !skills.is_empty() {
        registry.register(Arc::new(UseSkill::new(skills.clone())));
    }
    register_memory_tools(&mut registry, &memory, repository.clone());

    if agent_config.subagents.enabled {
        let mut child_registry = ToolRegistry::with_subagent_builtins();
        register_code_intelligence_tools(&mut child_registry, &intel);
        if !skills.is_empty() {
            child_registry.register(Arc::new(UseSkill::new(skills.clone())));
        }
        if let Some(memory) = &memory {
            child_registry.register(Arc::new(MemorySearchTool::new(memory.clone(), repository)));
        }
        // Named delegation tiers ([agent.subagents.models.<name>]) let the
        // root agent run mechanical child work on a cheaper model. Tier
        // resolution failures degrade to a note instead of killing the
        // session — the default (root model) path still works.
        let mut tiers = std::collections::BTreeMap::new();
        for (name, tier) in &agent_config.subagents.models {
            match ModelFactory::build_model(&tier.provider, &tier.model) {
                Ok(model) => {
                    tiers.insert(name.clone(), model);
                }
                Err(error) => {
                    events.emit(KodeEvent::Note {
                        text: format!(
                            "subagent model tier '{name}' unavailable: {error}; delegations targeting it will fail"
                        ),
                    });
                }
            }
        }
        if !tiers.is_empty() {
            events.emit(KodeEvent::Note {
                text: format!(
                    "subagent model tiers: [{}]",
                    tiers.keys().cloned().collect::<Vec<_>>().join(", ")
                ),
            });
        }
        registry.register(Arc::new(
            SubagentTool::new(
                model.clone(),
                child_registry,
                agent_config.clone(),
                effort.clone(),
                config.permissions.default_mode,
                handler.clone(),
                events.clone(),
                agent_config.subagents.max_result_chars,
            )
            .with_model_tiers(tiers)
            .with_system_appendix(skill_summary.clone())
            .with_cache_key(cache_key.clone()),
        ));
    }

    // Generic external MCP servers (kept architecturally separate from the
    // first-class Zindeks/Ingat integrations above). The session runtime
    // owns the server processes, so they survive from task to task and the
    // tools keep the same order in every request.
    let mut mcp_notes = Vec::new();
    let mcp_tools = runtime.mcp_tools(&config.mcp, &mut mcp_notes).await;
    for text in mcp_notes {
        events.emit(KodeEvent::Note { text });
    }
    for tool in mcp_tools {
        registry.register(tool);
    }

    let tools = ToolRuntime::new(registry, config.permissions.default_mode, handler.clone());
    let agent = Agent::new(model.clone(), tools, events.clone(), &agent_config)
        .with_effort(effort.clone())
        .with_system_appendix(skill_summary.clone())
        .with_cache_key(cache_key.clone());

    events.emit(KodeEvent::ContextCompilationStarted);
    let mut compiler =
        ContextCompiler::new(intel, memory, agent_config.context_budget_tokens as usize);
    if let Some(reranker) = reranker {
        compiler = compiler.with_reranker(
            reranker,
            Duration::from_millis(config.router.rerank_timeout_ms),
        );
    }
    let working_set = working_set_from(kode_context::git::repo_state(cwd).await);
    let compiled = compiler
        .compile(
            &ContextRequest {
                task: input.text.clone(),
                working_set,
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
        agent_config.context_budget_tokens as usize,
    ));
    events.emit(KodeEvent::Note {
        text: compiled.summary_line(),
    });
    if let Some(text) = crate::routing::rerank_note(&compiled.stats.rerank_status) {
        events.emit(KodeEvent::Note { text });
    }
    events.emit(KodeEvent::TaskProgress {
        step: TaskStep::Understand,
        done: true,
    });

    let initial_context = compiled.render();

    let (kept_history, history_truncated) =
        kode_agent::select_history(history, agent_config.history_budget_tokens as usize);
    if history_truncated {
        events.emit(KodeEvent::Note {
            text: "(older conversation truncated)".to_string(),
        });
    }

    // The prompt actually sent to the exec turn below: the original `task`
    // unless plan mode swaps in the approved-plan-injected version.
    let mut exec_task = input.clone();

    if plan_mode {
        let plan_result = run_plan_phase(
            model.clone(),
            &events,
            handler.clone(),
            &agent_config,
            effort.clone(),
            input,
            initial_context.as_deref(),
            kept_history,
            history_truncated,
            &ctx,
            steering.as_mut(),
            Some(skills.clone()),
            skill_summary.clone(),
            cache_key.clone(),
        )
        .await?;

        match plan_result {
            PlanOutcome::Rejected { outcome } => {
                close_and_defer_steering(&events, &mut steering);
                events.emit(KodeEvent::Note {
                    text: "plan rejected — task cancelled".to_string(),
                });
                events.emit(KodeEvent::TaskFinished {
                    iterations: outcome.iterations,
                    tool_calls: outcome.tool_calls,
                    input_tokens: outcome.usage.input_tokens,
                    output_tokens: outcome.usage.output_tokens,
                    cached_tokens: outcome.usage.cache_read_tokens,
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

    // Snapshot before the agent edits anything, so the task's ChangeSet
    // excludes changes that were already uncommitted.
    let changes_before = kode_context::git::change_snapshot(cwd).await;

    let outcome1 = agent
        .run_with_context_and_steering(
            &exec_task,
            initial_context.as_deref(),
            kept_history,
            history_truncated,
            &ctx,
            steering.as_mut(),
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
        let verification_service = VerificationService {
            root: cwd,
            config: &config.verify,
            cancel: &ctx.cancel,
        };
        let (report, verdict) = run_verification_phase(&verification_service, &events).await;
        verification = verification_status(verdict);

        if verdict == Verdict::Failed {
            events.emit(KodeEvent::Note {
                text: "verification failed — asking agent to fix".to_string(),
            });
            let retry_task = UserInput {
                text: format!(
                    "Verification failed after your previous changes. Fix the failures, then stop.\n\n{}\n\nOriginal task: {}",
                    report.render(),
                    input.text
                ),
                images: input.images.clone(),
            };

            // Repair must see the workspace produced by the first run, not
            // the pre-edit snapshot. Refresh code intelligence first (even
            // when a watcher exists, since it may not have observed the edit
            // yet), then compile a new git/intel/memory context.
            if let Some(adapter) = bound_backend.as_ref() {
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
            let repair_working_set = working_set_from(kode_context::git::repo_state(cwd).await);
            let repair_context = compiler
                .compile(
                    &ContextRequest {
                        task: retry_task.text.clone(),
                        working_set: repair_working_set,
                    },
                    cwd,
                )
                .await;
            events.emit(KodeEvent::ContextCompiled {
                token_estimate: repair_context.token_estimate(),
                sections: repair_context.sections.len(),
            });
            let repair_agent_context = repair_context.render();

            let retry_outcome = agent
                .run_with_context_and_steering(
                    &retry_task,
                    repair_agent_context.as_deref(),
                    kept_history,
                    history_truncated,
                    &ctx,
                    steering.as_mut(),
                )
                .await
                .map_err(|err| anyhow::anyhow!(err))?;

            let mutated_any = outcome1.mutated || retry_outcome.mutated;
            let mut retry_report = report;

            if mutated_any {
                let (report, verdict2) =
                    run_verification_phase(&verification_service, &events).await;
                retry_report = report;
                verification = verification_status(verdict2);
            }
            events.emit(KodeEvent::Note {
                text: retry_report.summary_line(),
            });

            outcome2 = Some(retry_outcome);
        }
    }

    let (iterations, tool_calls, usage, mutated_any) = match &outcome2 {
        Some(o2) => combine_outcomes(&outcome1, o2),
        None => (
            outcome1.iterations,
            outcome1.tool_calls,
            outcome1.usage,
            outcome1.mutated,
        ),
    };

    if mutated_any {
        emit_change_set(&events, changes_before.as_ref(), cwd).await;
    }

    if let Some(adapter) = bound_backend.as_ref()
        && mutated_any
    {
        if adapter.watching() {
            // The engine's own watcher is running and will pick up the
            // mutation on its own — no need for Kode to trigger a refresh.
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

    close_and_defer_steering(&events, &mut steering);
    events.emit(KodeEvent::TaskFinished {
        iterations,
        tool_calls,
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        cached_tokens: usage.cache_read_tokens,
    });

    Ok(TaskOutcome {
        status: TaskStatus::Completed,
        mutated: mutated_any,
        verification,
        repair_attempted: outcome2.is_some(),
        iterations,
        tool_calls,
        usage,
    })
}

fn close_and_defer_steering(
    events: &EventBus,
    steering: &mut Option<mpsc::UnboundedReceiver<UserInput>>,
) {
    let Some(receiver) = steering.as_mut() else {
        return;
    };
    receiver.close();
    let mut messages = Vec::new();
    while let Ok(message) = receiver.try_recv() {
        if !message.is_empty() {
            messages.push(message);
        }
    }
    if !messages.is_empty() {
        events.emit(KodeEvent::SteeringDeferred { messages });
    }
}

/// Outcome of [`run_plan_phase`]: the human's approve/reject answer to
/// "execute this plan?", carrying the plan turn's own `AgentOutcome` either
/// way — `run_task` reports it as the `TaskFinished` counters when the plan
/// is rejected (the exec turn never ran, so it's the only usage there is).
enum PlanOutcome {
    Approved {
        /// The exec-turn prompt: the approved plan text followed by the
        /// original task, per the "Follow this approved plan:" template.
        effective_task: UserInput,
    },
    Rejected {
        outcome: kode_agent::AgentOutcome,
    },
}

/// Runs the plan-mode turn with no implementation tools (only optional
/// read-only skill loading), streams a numbered plan for `task` into the
/// transcript via the normal `ModelToken` event path, then asks `handler` to approve or reject it via the same
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
    task: &UserInput,
    context: Option<&str>,
    history: &[kode_agent::HistoryTurn],
    history_truncated: bool,
    ctx: &ToolContext,
    steering: Option<&mut mpsc::UnboundedReceiver<UserInput>>,
    skills: Option<Arc<SkillCatalog>>,
    system_appendix: Option<String>,
    cache_key: Option<String>,
) -> anyhow::Result<PlanOutcome> {
    // No implementation tools are offered on this turn. `Deny` still permits
    // the optional read-only `use_skill` tool.
    let mut registry = ToolRegistry::new();
    if let Some(skills) = skills {
        registry.register(Arc::new(UseSkill::new(skills)));
    }
    let tools = ToolRuntime::new(registry, PermissionMode::Deny, handler.clone());
    let plan_agent = Agent::new(model, tools, events.clone(), agent_cfg)
        .with_effort(effort)
        .with_system_appendix(system_appendix)
        .with_cache_key(cache_key);

    let plan_prompt = UserInput {
        text: format!("{}\n\n{PLAN_INSTRUCTION}", task.text),
        images: task.images.clone(),
    };
    let outcome = plan_agent
        .run_with_context_and_steering(
            &plan_prompt,
            context,
            history,
            history_truncated,
            ctx,
            steering,
        )
        .await
        .map_err(|err| anyhow::anyhow!(err))?;

    let approved = handler.confirm("execute this plan?").await;
    if approved {
        let plan_text = outcome.final_text.trim().to_string();
        let effective_task = UserInput {
            text: format!(
                "Follow this approved plan:\n{plan_text}\n\nTask: {}",
                task.text
            ),
            images: task.images.clone(),
        };
        Ok(PlanOutcome::Approved { effective_task })
    } else {
        Ok(PlanOutcome::Rejected { outcome })
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
/// usage, and `mutated` OR'd across both runs (a mutation in either run
/// means the workspace changed).
fn combine_outcomes(
    a: &kode_agent::AgentOutcome,
    b: &kode_agent::AgentOutcome,
) -> (u32, u32, Usage, bool) {
    let mut usage = a.usage;
    usage += b.usage;
    (
        a.iterations + b.iterations,
        a.tool_calls + b.tool_calls,
        usage,
        a.mutated || b.mutated,
    )
}

const WORKING_SET_MAX: usize = 20;

/// Files with uncommitted changes: the best available signal for what the
/// user is working on. Code retrieval ranks toward them. Empty outside a
/// git repository.
fn working_set_from(state: Option<kode_context::git::RepoState>) -> Vec<String> {
    state
        .map(|state| {
            state
                .numstat
                .into_iter()
                .take(WORKING_SET_MAX)
                .map(|row| row.path)
                .collect()
        })
        .unwrap_or_default()
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

/// Emits the task's `ChangeSet` from a start snapshot and a fresh one.
/// Outside a repository (either snapshot missing) it says so instead.
async fn emit_change_set(
    events: &EventBus,
    before: Option<&kode_context::git::ChangeSnapshot>,
    cwd: &Path,
) {
    let after = kode_context::git::change_snapshot(cwd).await;
    match (before, after) {
        (Some(before), Some(after)) => {
            let files = kode_context::git::change_set(before, &after)
                .into_iter()
                .map(|row| kode_core::event::FileChange {
                    path: row.path,
                    added: row.added,
                    removed: row.deleted,
                })
                .collect();
            events.emit(KodeEvent::ChangeSet { files });
        }
        _ => events.emit(KodeEvent::Note {
            text: "changed files not recorded: git unavailable or not a repository".to_string(),
        }),
    }
}

#[cfg(test)]
mod plan_phase_tests {
    use super::*;
    use kode_model::{FinishReason, Message, MockModel, StreamEvent};
    use kode_tools::permission::{AutoApprove, AutoDeny};

    #[test]
    fn cache_keys_are_process_scoped_and_distinct() {
        assert!(new_cache_key().starts_with(&format!("kode-{}-", std::process::id())));
        // Back-to-back calls can land in the same clock tick (CI runners
        // report microseconds); keys must still differ.
        assert_ne!(cache_key_at(42), cache_key_at(42));
    }

    #[tokio::test]
    async fn plan_phase_forwards_appendix_and_cache_key() {
        let dir = temp_dir("hints");
        let mock = Arc::new(MockModel::new());
        mock.push_script(plan_script("1. do the thing"));

        run_plan_phase(
            mock.clone(),
            &EventBus::new(64),
            Arc::new(AutoApprove),
            &AgentConfig::default(),
            None,
            &UserInput::text("add a widget"),
            Some("CTX"),
            &[],
            false,
            &ctx(dir),
            None,
            None,
            Some("SKILL_CATALOG".to_string()),
            Some("kode-plan".to_string()),
        )
        .await
        .unwrap();

        let requests = mock.requests();
        assert_eq!(requests[0].cache_key.as_deref(), Some("kode-plan"));
        assert!(matches!(
            &requests[0].messages[0],
            Message::System(text) if text.ends_with("\n\nSKILL_CATALOG")
        ));
    }

    #[test]
    fn closing_steering_returns_unconsumed_messages() {
        let events = EventBus::new(8);
        let mut event_rx = events.subscribe();
        let (tx, rx) = mpsc::unbounded_channel();
        tx.send(UserInput::text("late direction")).unwrap();
        let mut steering = Some(rx);

        close_and_defer_steering(&events, &mut steering);

        assert!(tx.send(UserInput::text("too late")).is_err());
        assert!(matches!(
            event_rx.try_recv().unwrap(),
            KodeEvent::SteeringDeferred { messages }
                if messages == vec![UserInput::text("late direction")]
        ));
    }

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
            &UserInput::text("add a widget"),
            None,
            &[],
            false,
            &ctx(dir),
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();

        match outcome {
            PlanOutcome::Approved { effective_task } => {
                assert!(
                    effective_task
                        .text
                        .starts_with("Follow this approved plan:\n")
                );
                assert!(
                    effective_task
                        .text
                        .contains("1. do the thing\n2. verify it")
                );
                assert!(effective_task.text.ends_with("\n\nTask: add a widget"));
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
            &UserInput::text("add a widget"),
            None,
            &[],
            false,
            &ctx(dir),
            None,
            None,
            None,
            None,
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

                ..Default::default()
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
        let mut a = outcome(3, 5, 100, 200, true);
        a.usage.cache_read_tokens = Some(60);
        let b = outcome(2, 4, 50, 75, false);

        let (iterations, tool_calls, usage, mutated) = combine_outcomes(&a, &b);

        assert_eq!(iterations, 5);
        assert_eq!(tool_calls, 9);
        assert_eq!(usage.input_tokens, 150);
        assert_eq!(usage.output_tokens, 275);
        assert_eq!(usage.cache_read_tokens, Some(60));
        assert!(mutated);
    }

    #[test]
    fn combine_outcomes_mutated_false_when_neither_mutated() {
        let a = outcome(1, 1, 10, 10, false);
        let b = outcome(1, 1, 10, 10, false);

        let (.., usage, mutated) = combine_outcomes(&a, &b);
        assert!(!mutated);
        assert_eq!(usage.cache_read_tokens, None);
    }

    fn row(path: &str) -> kode_context::git::NumstatRow {
        kode_context::git::NumstatRow {
            path: path.to_string(),
            added: 1,
            deleted: 0,
        }
    }

    #[test]
    fn working_set_is_empty_outside_a_repository() {
        assert!(working_set_from(None).is_empty());
    }

    #[test]
    fn working_set_lists_changed_files_in_git_order() {
        let state = kode_context::git::RepoState {
            dirty: true,
            numstat: vec![row("src/b.rs"), row("src/a.rs")],
        };
        assert_eq!(working_set_from(Some(state)), vec!["src/b.rs", "src/a.rs"]);
    }

    #[test]
    fn working_set_is_capped() {
        let state = kode_context::git::RepoState {
            dirty: true,
            numstat: (0..50).map(|n| row(&format!("src/f{n}.rs"))).collect(),
        };
        let set = working_set_from(Some(state));
        assert_eq!(set.len(), WORKING_SET_MAX);
        assert_eq!(set[0], "src/f0.rs");
    }

    #[tokio::test]
    async fn working_set_of_a_non_repository_directory_is_empty() {
        let dir = std::env::temp_dir().join(format!(
            "kode-working-set-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(working_set_from(kode_context::git::repo_state(&dir).await).is_empty());
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

    #[test]
    fn available_code_intelligence_registers_search_and_outline_tools() {
        let intel: Option<Arc<dyn CodeIntelligence>> =
            Some(Arc::new(kode_intel::MockCodeIntelligence::default()));
        let mut registry = ToolRegistry::new();

        register_code_intelligence_tools(&mut registry, &intel);

        let names = registry
            .specs()
            .into_iter()
            .map(|tool| tool.name)
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["code_search", "file_outline"]);
    }

    #[test]
    fn available_memory_registers_search_and_remember_tools() {
        let memory: Option<Arc<dyn EngineeringMemory>> =
            Some(Arc::new(kode_memory::MockEngineeringMemory::default()));
        let mut registry = ToolRegistry::new();

        register_memory_tools(&mut registry, &memory, Some("kode".to_string()));

        let names = registry
            .specs()
            .into_iter()
            .map(|tool| tool.name)
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["memory_search", "remember"]);
    }
}

#[cfg(test)]
mod change_set_tests {
    use super::*;

    #[tokio::test]
    async fn emit_change_set_outside_a_repo_notes_instead() {
        let dir = std::env::temp_dir().join(format!(
            "kode-emit-change-set-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let bus = EventBus::new(8);
        let mut rx = bus.subscribe();
        emit_change_set(&bus, None, &dir).await;
        match rx.recv().await.unwrap() {
            KodeEvent::Note { text } => assert!(text.contains("changed files not recorded")),
            other => panic!("expected Note, got {other:?}"),
        }
    }
}
