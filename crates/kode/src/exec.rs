use std::io::Write;
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

struct StdinPermission;

#[async_trait::async_trait]
impl PermissionHandler for StdinPermission {
    async fn confirm(&self, summary: &str) -> bool {
        eprint!("kode wants to run: {summary} — allow? [y/N] ");
        let _ = std::io::stderr().flush();
        let answer = tokio::task::spawn_blocking(|| {
            let mut line = String::new();
            let _ = std::io::stdin().read_line(&mut line);
            line
        })
        .await
        .unwrap_or_default();
        matches!(answer.trim().to_lowercase().as_str(), "y" | "yes")
    }
}

pub async fn run(task: &str, cwd: &Path, cancel: CancellationToken) -> anyhow::Result<()> {
    let config = KodeConfig::load(cwd)?;

    if config.model.provider != "openai" {
        anyhow::bail!("provider {} not supported yet", config.model.provider);
    }

    let api_key = std::env::var("OPENAI_API_KEY")
        .or_else(|_| std::env::var("KODE_API_KEY"))
        .map_err(|_| anyhow::anyhow!("set OPENAI_API_KEY to run `kode exec`"))?;

    if config.model.model.is_empty() {
        anyhow::bail!("set model.model in .kode/config.toml");
    }

    let mut opts = OpenAiOptions {
        api_key,
        model: config.model.model.clone(),
        ..Default::default()
    };
    if let Ok(base_url) = std::env::var("OPENAI_BASE_URL") {
        opts.base_url = base_url;
    }

    let model: Arc<dyn kode_model::Model> = Arc::new(OpenAiModel::new(opts));
    let events = EventBus::new(256);
    let mut rx = events.subscribe();

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
                        eprintln!("◆ code intelligence unavailable: {e}");
                        None
                    }
                }
            }
            Err(e) => {
                eprintln!("◆ code intelligence unavailable: {e}");
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
                eprintln!("◆ engineering memory unavailable: {e}");
                None
            }
            Err(_) => {
                eprintln!("◆ engineering memory unavailable: request timed out");
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
    let tools = ToolRuntime::new(
        registry,
        config.permissions.default_mode,
        Arc::new(StdinPermission),
    );
    let agent = Agent::new(model, tools, events.clone(), &config.agent);

    let printer = tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(KodeEvent::ModelToken { text }) => {
                    print!("{text}");
                    let _ = std::io::stdout().flush();
                }
                Ok(KodeEvent::ToolStarted { name }) => {
                    eprintln!("◆ {name}");
                }
                Ok(KodeEvent::ToolFinished { name, ok: false }) => {
                    eprintln!("◆ {name} failed");
                }
                Ok(KodeEvent::AgentError { message }) => {
                    eprintln!("{message}");
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
    });

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
    eprintln!("◆ {}", compiled.summary_line());

    let mut outcome = match agent
        .run_with_context(task, compiled.render().as_deref(), &ctx)
        .await
    {
        Ok(outcome) => outcome,
        Err(err) => {
            drop(agent);
            let _ = printer.await;
            return Err(anyhow::anyhow!(err));
        }
    };

    let mut final_mutated = outcome.mutated;

    if outcome.mutated {
        let profile = kode_verify::detect(cwd);
        events.emit(KodeEvent::VerificationStarted);
        let mut report = kode_verify::run_verification(cwd, &profile, &ctx.cancel).await;
        print_step_lines(&report);
        events.emit(KodeEvent::VerificationFinished { ok: report.ok });

        if !report.ok {
            eprintln!("◆ verification failed — asking agent to fix");
            let retry_task = format!(
                "Verification failed after your previous changes. Fix the failures, then stop.\n\n{}\n\nOriginal task: {}",
                report.render(),
                task
            );

            match agent
                .run_with_context(&retry_task, compiled.render().as_deref(), &ctx)
                .await
            {
                Ok(retry_outcome) => {
                    final_mutated = retry_outcome.mutated;
                    outcome = retry_outcome;

                    if outcome.mutated {
                        let profile = kode_verify::detect(cwd);
                        events.emit(KodeEvent::VerificationStarted);
                        report = kode_verify::run_verification(cwd, &profile, &ctx.cancel).await;
                        print_step_lines(&report);
                        events.emit(KodeEvent::VerificationFinished { ok: report.ok });
                    }
                    eprintln!("◆ {}", report.summary_line());
                }
                Err(err) => {
                    drop(agent);
                    let _ = printer.await;
                    return Err(anyhow::anyhow!(err));
                }
            }
        }
    }

    if let Some(adapter) = zindeks_adapter.as_ref()
        && final_mutated
    {
        match adapter.ensure_bound().await {
            Ok(()) => eprintln!("◆ zindeks index refreshed"),
            Err(e) => eprintln!("◆ zindeks refresh failed (non-fatal): {e}"),
        }
    }

    drop(agent);
    let _ = printer.await;

    println!();
    eprintln!(
        "— {} iterations, {} tool calls, {}→{} tokens",
        outcome.iterations,
        outcome.tool_calls,
        outcome.usage.input_tokens,
        outcome.usage.output_tokens
    );
    Ok(())
}

fn print_step_lines(report: &kode_verify::VerificationReport) {
    for step in &report.steps {
        let tag = match &step.status {
            kode_verify::StepStatus::Passed => "PASS",
            kode_verify::StepStatus::Failed => "FAIL",
            kode_verify::StepStatus::Skipped(_) => "SKIP",
        };
        eprintln!("◆ {}: {tag}", step.name);
    }
}
