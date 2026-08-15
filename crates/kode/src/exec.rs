use std::io::Write;
use std::path::Path;
use std::sync::Arc;

use kode_core::CancellationToken;
use kode_core::config::KodeConfig;
use kode_core::event::{EventBus, KodeEvent};
use kode_tools::permission::PermissionHandler;

use crate::pipeline;

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

pub async fn run(
    task: &str,
    cwd: &Path,
    cancel: CancellationToken,
    model_override: Option<String>,
    effort_override: Option<String>,
) -> anyhow::Result<()> {
    let mut config = KodeConfig::load(cwd)?;
    if let Some(model) = model_override {
        config.model.model = model;
    }
    if let Some(effort) = effort_override {
        config.model.effort = effort;
    }

    let events = EventBus::new(256);
    let mut rx = events.subscribe();

    let printer = tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(KodeEvent::ModelToken { text }) => {
                    print!("{text}");
                    let _ = std::io::stdout().flush();
                }
                Ok(KodeEvent::Note { text }) => {
                    eprintln!("◆ {text}");
                }
                Ok(KodeEvent::VerifyStep {
                    name,
                    passed,
                    skipped,
                    ..
                }) => {
                    let tag = if skipped {
                        "SKIP"
                    } else if passed {
                        "PASS"
                    } else {
                        "FAIL"
                    };
                    eprintln!("◆ {name}: {tag}");
                }
                Ok(KodeEvent::Knowledge {
                    zindeks,
                    ingat,
                    git,
                    ..
                }) => {
                    eprintln!(
                        "◆ knows: Z:{} I:{} G:{}",
                        zindeks.len(),
                        ingat.len(),
                        git.len()
                    );
                }
                Ok(KodeEvent::TaskFinished {
                    iterations,
                    tool_calls,
                    input_tokens,
                    output_tokens,
                }) => {
                    eprintln!(
                        "— {} iterations, {} tool calls, {}→{} tokens",
                        iterations, tool_calls, input_tokens, output_tokens
                    );
                }
                Ok(KodeEvent::AgentError { message }) => {
                    eprintln!("{message}");
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
    });

    let result = pipeline::run_task(
        task,
        cwd,
        &config,
        events,
        Arc::new(StdinPermission),
        cancel,
    )
    .await;

    let _ = printer.await;

    if result.is_ok() {
        println!();
    }
    result
}
