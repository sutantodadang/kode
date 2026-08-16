use std::io::Write;
use std::path::Path;
use std::sync::Arc;

use kode_core::CancellationToken;
use kode_core::config::KodeConfig;
use kode_core::event::{EventBus, KodeEvent};
use kode_tools::permission::PermissionHandler;
use tokio::sync::broadcast::error::RecvError;

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
                Err(RecvError::Closed) => break,
                Err(RecvError::Lagged(n)) => {
                    eprintln!("{}", lagged_note(n));
                    // The broadcast channel dropped events out from under a
                    // slow receiver — keep printing rather than treating a
                    // lag as a stream close.
                }
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
        &[],
    )
    .await;

    let _ = printer.await;

    if result.is_ok() {
        println!();
    }
    result
}

/// Message printed when the event printer falls behind and the broadcast
/// channel drops events (`RecvError::Lagged`). Factored out so the message
/// is unit-testable without driving an actual broadcast channel.
fn lagged_note(n: u64) -> String {
    format!("◆ event stream lagged — {n} events dropped")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lagged_note_reports_dropped_count() {
        assert_eq!(lagged_note(7), "◆ event stream lagged — 7 events dropped");
    }
}
