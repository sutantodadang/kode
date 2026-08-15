use std::path::Path;
use std::sync::Arc;

use kode_agent::Agent;
use kode_context::{ContextCompiler, ContextRequest};
use kode_core::CancellationToken;
use kode_core::config::KodeConfig;
use kode_core::event::{EventBus, KodeEvent};
use kode_intel::{CodeIntelligence, ZindeksAdapter};
use kode_memory::{EngineeringMemory, IngatAdapter, RememberTool};
use kode_model::{OpenAiModel, OpenAiOptions};
use kode_tools::ToolContext;
use kode_tools::permission::PermissionHandler;
use kode_tools::registry::{ToolRegistry, ToolRuntime};

/// Runs one agentic task end-to-end: model/config setup, code intelligence
/// and engineering memory binding, context compilation, the agent loop, and
/// post-edit verification with a single retry. This is the single code path
/// shared by `kode exec` (headless) and the TUI — it communicates *only*
/// through `events`, never via stdout/stderr directly.
pub async fn run_task(
    task: &str,
    cwd: &Path,
    config: &KodeConfig,
    events: EventBus,
    handler: Arc<dyn PermissionHandler>,
    cancel: CancellationToken,
) -> anyhow::Result<()> {
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
        other => anyhow::bail!(
            "provider {other} not supported yet (supported: openai, codex, opencode-go, opencode, kilo, lmstudio)"
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
                        events.emit(KodeEvent::Note {
                            text: format!("code intelligence unavailable: {e}"),
                        });
                        None
                    }
                }
            }
            Err(e) => {
                events.emit(KodeEvent::Note {
                    text: format!("code intelligence unavailable: {e}"),
                });
                None
            }
        }
    } else {
        None
    };

    let memory: Option<Arc<dyn EngineeringMemory>> = if config.ingat.enabled {
        let adapter = IngatAdapter::new(&config.ingat);
        match tokio::time::timeout(std::time::Duration::from_secs(3), adapter.health()).await {
            Ok(Ok(())) => Some(Arc::new(adapter) as Arc<dyn EngineeringMemory>),
            Ok(Err(e)) => {
                events.emit(KodeEvent::Note {
                    text: format!("engineering memory unavailable: {e}"),
                });
                None
            }
            Err(_) => {
                events.emit(KodeEvent::Note {
                    text: "engineering memory unavailable: request timed out".to_string(),
                });
                None
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

    let tools = ToolRuntime::new(registry, config.permissions.default_mode, handler);
    let agent = Agent::new(model, tools, events.clone(), &config.agent);

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
    events.emit(KodeEvent::Note {
        text: compiled.summary_line(),
    });

    let mut outcome = agent
        .run_with_context(task, compiled.render().as_deref(), &ctx)
        .await
        .map_err(|err| anyhow::anyhow!(err))?;

    let mut final_mutated = outcome.mutated;

    if outcome.mutated {
        let profile = kode_verify::detect(cwd);
        events.emit(KodeEvent::VerificationStarted);
        let mut report = kode_verify::run_verification(cwd, &profile, &ctx.cancel).await;
        emit_step_lines(&events, &report);
        events.emit(KodeEvent::VerificationFinished { ok: report.ok });

        if !report.ok {
            events.emit(KodeEvent::Note {
                text: "verification failed — asking agent to fix".to_string(),
            });
            let retry_task = format!(
                "Verification failed after your previous changes. Fix the failures, then stop.\n\n{}\n\nOriginal task: {}",
                report.render(),
                task
            );

            let retry_outcome = agent
                .run_with_context(&retry_task, compiled.render().as_deref(), &ctx)
                .await
                .map_err(|err| anyhow::anyhow!(err))?;

            final_mutated = retry_outcome.mutated;
            outcome = retry_outcome;

            if outcome.mutated {
                let profile = kode_verify::detect(cwd);
                events.emit(KodeEvent::VerificationStarted);
                report = kode_verify::run_verification(cwd, &profile, &ctx.cancel).await;
                emit_step_lines(&events, &report);
                events.emit(KodeEvent::VerificationFinished { ok: report.ok });
            }
            events.emit(KodeEvent::Note {
                text: report.summary_line(),
            });
        }
    }

    if let Some(adapter) = zindeks_adapter.as_ref()
        && final_mutated
    {
        match adapter.ensure_bound().await {
            Ok(()) => events.emit(KodeEvent::Note {
                text: "zindeks index refreshed".to_string(),
            }),
            Err(e) => events.emit(KodeEvent::Note {
                text: format!("zindeks refresh failed (non-fatal): {e}"),
            }),
        }
    }

    events.emit(KodeEvent::TaskFinished {
        iterations: outcome.iterations,
        tool_calls: outcome.tool_calls,
        input_tokens: outcome.usage.input_tokens,
        output_tokens: outcome.usage.output_tokens,
    });

    Ok(())
}

fn emit_step_lines(events: &EventBus, report: &kode_verify::VerificationReport) {
    for step in &report.steps {
        let tag = match &step.status {
            kode_verify::StepStatus::Passed => "PASS",
            kode_verify::StepStatus::Failed => "FAIL",
            kode_verify::StepStatus::Skipped(_) => "SKIP",
        };
        events.emit(KodeEvent::Note {
            text: format!("{}: {tag}", step.name),
        });
    }
}
