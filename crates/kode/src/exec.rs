use std::io::Write;
use std::path::Path;
use std::sync::Arc;

use kode_core::CancellationToken;
use kode_core::config::KodeConfig;
use kode_core::event::{EventBus, KodeEvent};
use kode_memory::EngineeringMemory;
use kode_tools::permission::PermissionHandler;
use tokio::sync::broadcast::error::RecvError;

use crate::custom_commands;
use crate::pipeline;
use crate::session;
use crate::team_memory;

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
    continue_session: bool,
    plan_mode: bool,
) -> anyhow::Result<()> {
    let mut config = KodeConfig::load(cwd)?;
    if let Some(model) = model_override {
        config.model.model = model;
    }
    if let Some(effort) = effort_override {
        config.model.effort = effort;
    }

    if config.ingat.enabled {
        let adapter = kode_memory::IngatAdapter::new(&config.ingat);
        if tokio::time::timeout(std::time::Duration::from_secs(3), adapter.health())
            .await
            .is_ok_and(|r| r.is_ok())
        {
            let summary = team_memory::import_on_start(&adapter, cwd).await;
            if let Some(text) = summary.note() {
                eprintln!("◆ {text}");
            }
        }
    }

    let expanded_task;
    let task: &str = if task.trim_start().starts_with('/') {
        expanded_task = resolve_custom_task(task, cwd)?;
        &expanded_task
    } else {
        task
    };

    let session_id = if continue_session {
        session::latest(cwd)
    } else {
        None
    };
    let mut history_turns: Vec<kode_agent::HistoryTurn> = Vec::new();
    if continue_session {
        if let Some(id) = &session_id {
            match session::load(cwd, id) {
                Ok((turns, corrupt)) => {
                    if corrupt > 0 {
                        println!("session {id}: skipped {corrupt} corrupt lines");
                    }
                    history_turns = turns
                        .into_iter()
                        .map(|t| kode_agent::HistoryTurn {
                            task: t.task,
                            response: t.response,
                        })
                        .collect();
                }
                Err(e) => println!("could not load session {id}: {e}"),
            }
        } else {
            println!("no previous session — starting fresh");
        }
    }

    let events = EventBus::new(256);
    let mut rx = events.subscribe();

    let printer = tokio::spawn(async move {
        // Mirrors how the TUI's `AppState.response_buf` accumulates flushed
        // model text across a task's run and snapshots it into
        // `last_response` on `TaskFinished` — see `tui.rs::apply_event`.
        let mut response_buf = String::new();
        let mut final_tool_calls: u32 = 0;
        loop {
            match rx.recv().await {
                Ok(KodeEvent::ModelToken { text }) => {
                    print!("{text}");
                    let _ = std::io::stdout().flush();
                    response_buf.push_str(&text);
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
                    final_tool_calls = tool_calls;
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
        (response_buf, final_tool_calls)
    });

    let result = pipeline::run_task(
        task,
        cwd,
        &config,
        events,
        Arc::new(StdinPermission),
        cancel,
        &history_turns,
        plan_mode,
    )
    .await;

    let (final_text, tool_calls) = printer.await.unwrap_or_default();

    if result.is_ok() {
        println!();
        if continue_session {
            let id = match session_id.clone() {
                Some(id) => id,
                None => session::create(cwd, &config.model.provider, &config.model.model)?,
            };
            let (_, ts) = session::now_utc_stamp();
            let turn = session::Turn {
                ts,
                task: task.to_string(),
                response: final_text,
                tool_calls,
            };
            if let Err(e) = session::append_turn(cwd, &id, &turn) {
                println!("session append failed (non-fatal): {e}");
            }
        }
    }
    let outcome = result?;
    if !outcome.is_success() {
        match outcome.status {
            pipeline::TaskStatus::Cancelled => anyhow::bail!("task cancelled"),
            pipeline::TaskStatus::Completed => match outcome.verification {
                pipeline::VerificationStatus::Failed => {
                    anyhow::bail!("verification failed after repair")
                }
                pipeline::VerificationStatus::NoChecks => {
                    anyhow::bail!("changes are unverified: no verification checks ran")
                }
                _ => anyhow::bail!("task did not complete successfully"),
            },
        }
    }
    Ok(())
}

/// Message printed when the event printer falls behind and the broadcast
/// channel drops events (`RecvError::Lagged`). Factored out so the message
/// is unit-testable without driving an actual broadcast channel.
fn lagged_note(n: u64) -> String {
    format!("◆ event stream lagged — {n} events dropped")
}

/// Resolves a `/`-prefixed TASK into its expanded custom-command prompt by
/// looking it up against commands discovered under `.kode/commands` and
/// `~/.kode/commands`. Errors (never panics) when the name doesn't match
/// any discovered command — listing the available ones — or when the
/// matched template file can't be read.
fn resolve_custom_task(task: &str, cwd: &Path) -> anyhow::Result<String> {
    let trimmed = task.trim();
    let mut parts = trimmed.splitn(2, char::is_whitespace);
    let cmd = parts.next().unwrap_or("");
    let args = parts.next().unwrap_or("").trim();
    let name = cmd.trim_start_matches('/').to_lowercase();

    let commands = custom_commands::discover(cwd, &[]);
    match commands.iter().find(|c| c.name == name) {
        Some(found) => Ok(custom_commands::expand(&found.path, args)?),
        None => {
            let available = commands
                .iter()
                .map(|c| format!("/{}", c.name))
                .collect::<Vec<_>>()
                .join(", ");
            if available.is_empty() {
                anyhow::bail!(
                    "unknown command '/{name}' (no custom commands found in .kode/commands or ~/.kode/commands)"
                );
            } else {
                anyhow::bail!("unknown command '/{name}' (available: {available})");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lagged_note_reports_dropped_count() {
        assert_eq!(lagged_note(7), "◆ event stream lagged — 7 events dropped");
    }

    fn temp_dir(label: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "kode-exec-test-{label}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn resolve_custom_task_expands_matched_command() {
        let dir = temp_dir("match");
        let cmds = dir.join(".kode").join("commands");
        std::fs::create_dir_all(&cmds).unwrap();
        std::fs::write(cmds.join("review.md"), "Review: $ARGUMENTS").unwrap();

        let expanded = resolve_custom_task("/review the diff", &dir).unwrap();
        assert_eq!(expanded, "Review: the diff");
    }

    #[test]
    fn resolve_custom_task_unknown_name_lists_available() {
        let dir = temp_dir("unknown");
        let cmds = dir.join(".kode").join("commands");
        std::fs::create_dir_all(&cmds).unwrap();
        std::fs::write(cmds.join("review.md"), "Review: $ARGUMENTS").unwrap();

        let err = resolve_custom_task("/nope", &dir).unwrap_err();
        assert!(err.to_string().contains("unknown command '/nope'"));
        assert!(err.to_string().contains("/review"));
    }

    #[test]
    fn resolve_custom_task_unknown_name_no_commands_found() {
        let dir = temp_dir("empty");
        let err = resolve_custom_task("/nope", &dir).unwrap_err();
        assert!(err.to_string().contains("no custom commands found"));
    }
}
