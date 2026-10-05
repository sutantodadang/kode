use std::io::Write;
use std::path::Path;
use std::sync::Arc;

use kode_core::CancellationToken;
use kode_core::config::KodeConfig;
use kode_core::event::{EventBus, KodeEvent, router_summary};
use kode_tools::permission::PermissionHandler;

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

#[allow(clippy::too_many_arguments)]
pub async fn run(
    task: &str,
    cwd: &Path,
    cancel: CancellationToken,
    model_override: Option<String>,
    effort_override: Option<String>,
    continue_session: bool,
    plan_mode: bool,
    image_paths: &[std::path::PathBuf],
    no_graph_answer: bool,
    propose_memory: bool,
    save_memory: bool,
) -> anyhow::Result<()> {
    validate_memory_flags(propose_memory, save_memory).map_err(|e| anyhow::anyhow!(e))?;
    let mut config = KodeConfig::load(cwd)?;
    if let Some(model) = model_override {
        config.model.model = model;
    }
    if let Some(effort) = effort_override {
        config.model.effort = effort;
    }

    if let Ok(Some(adapter)) = crate::memory_backend::connect(&config.ingat).await
        && tokio::time::timeout(std::time::Duration::from_secs(3), adapter.health())
            .await
            .is_ok_and(|r| r.is_ok())
    {
        let summary = team_memory::import_on_start(adapter.as_ref(), cwd).await;
        if let Some(text) = summary.note() {
            eprintln!("◆ {text}");
        }
    }

    let expanded_task;
    let task: &str = if task.trim_start().starts_with('/') {
        expanded_task = resolve_custom_task(task, cwd)?;
        &expanded_task
    } else {
        task
    };

    if image_paths.len() > crate::attachments::MAX_IMAGES {
        anyhow::bail!("too many images; maximum is 20 per turn");
    }
    let images = image_paths
        .iter()
        .map(|path| crate::attachments::load_image(cwd, &path.to_string_lossy()))
        .collect::<anyhow::Result<Vec<_>>>()?;
    let total_image_bytes = images.iter().map(|image| image.size_bytes).sum::<usize>();
    if total_image_bytes > crate::attachments::MAX_TOTAL_IMAGE_BYTES {
        anyhow::bail!("images exceed the 20 MiB total limit");
    }
    let input = kode_core::UserInput {
        text: task.to_string(),
        images,
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
                            images: t.images,
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
    let mut rx = events.subscribe_lossless();

    let printer = tokio::spawn(async move {
        // Mirrors how the TUI's `AppState.response_buf` accumulates flushed
        // model text across a task's run and snapshots it into
        // `last_response` on `TaskFinished` — see `tui.rs::apply_event`.
        let mut response_buf = String::new();
        let mut final_tool_calls: u32 = 0;
        let mut recorder = crate::ledger::LedgerRecorder::default();
        recorder.begin();
        while let Some(ev) = rx.recv().await {
            {
                {
                    recorder.observe(&ev);
                    match ev {
                        KodeEvent::ModelToken { text } => {
                            print!("{text}");
                            let _ = std::io::stdout().flush();
                            response_buf.push_str(&text);
                        }
                        KodeEvent::Note { text } => {
                            eprintln!("◆ {text}");
                        }
                        KodeEvent::RouterDecision { answers } => {
                            eprintln!("◆ {}", router_summary(&answers));
                        }
                        KodeEvent::VerifyStep {
                            name,
                            passed,
                            skipped,
                            ..
                        } => {
                            let tag = if skipped {
                                "SKIP"
                            } else if passed {
                                "PASS"
                            } else {
                                "FAIL"
                            };
                            eprintln!("◆ {name}: {tag}");
                        }
                        KodeEvent::Knowledge {
                            zindeks,
                            ingat,
                            git,
                            ..
                        } => {
                            eprintln!(
                                "◆ knows: Z:{} I:{} G:{}",
                                zindeks.len(),
                                ingat.len(),
                                git.len()
                            );
                        }
                        KodeEvent::SubagentStarted { id, ownership } => {
                            let scope = if ownership.is_empty() {
                                "read-only".to_string()
                            } else {
                                format!("owns {}", ownership.join(", "))
                            };
                            eprintln!("◆ subagent {id}: started ({scope})");
                        }
                        KodeEvent::SubagentActivity { id, text } => {
                            eprintln!("  ├─ {id}: {text}");
                        }
                        KodeEvent::SubagentFinished { id, ok, summary } => {
                            eprintln!(
                                "◆ subagent {id}: {} — {summary}",
                                if ok { "done" } else { "failed" }
                            );
                        }
                        KodeEvent::TaskFinished {
                            iterations,
                            tool_calls,
                            input_tokens,
                            output_tokens,
                            cached_tokens,
                        } => {
                            let cached = match cached_tokens {
                                Some(tokens) => format!("{tokens} cached"),
                                None => "cached ? not reported".to_string(),
                            };
                            eprintln!(
                                "— {iterations} iterations, {tool_calls} tool calls, {input_tokens}→{output_tokens} tokens ({cached})"
                            );
                            final_tool_calls = tool_calls;
                        }
                        KodeEvent::AgentError { message } => {
                            eprintln!("{message}");
                        }
                        KodeEvent::ChangeSet { files, .. } => {
                            for file in &files {
                                eprintln!(
                                    "◆ change {} +{} −{}",
                                    file.path, file.added, file.removed
                                );
                            }
                        }
                        KodeEvent::IndexStarted => eprintln!("◆ indexing…"),
                        KodeEvent::IndexFinished { error: Some(e), .. } => {
                            eprintln!("◆ index failed: {e}")
                        }
                        KodeEvent::IndexFinished { .. } => eprintln!("◆ indexed"),
                        KodeEvent::GraphAnswered {
                            text, latency_ms, ..
                        } => {
                            println!("{text}");
                            eprintln!("— graph answer · 0 tokens · {latency_ms}ms");
                            response_buf.push_str(&text);
                        }
                        KodeEvent::Impact {
                            symbol,
                            callers,
                            tests,
                            ..
                        } => {
                            eprintln!("◆ impact {symbol}: {callers} callers, {tests} tests")
                        }
                        _ => {}
                    }
                }
            }
        }
        (response_buf, final_tool_calls, recorder.take_turn())
    });

    let runtime = crate::session_runtime::SessionRuntime::new();
    let result = pipeline::run_task_with_input(
        &input,
        cwd,
        &config,
        events,
        Arc::new(StdinPermission),
        cancel,
        &history_turns,
        plan_mode,
        None,
        Some(pipeline::new_cache_key()),
        &runtime,
        !no_graph_answer,
    )
    .await;

    let (final_text, tool_calls, (ledger, signals)) = printer.await.unwrap_or_default();

    let mut saved_id: Option<String> = None;
    if result.is_ok() {
        println!();
        match session_for_run(
            cwd,
            continue_session,
            &config.model.provider,
            &config.model.model,
        ) {
            Ok(id) => {
                let (_, ts) = session::now_utc_stamp();
                let turn = session::Turn {
                    ts,
                    task: input.text.clone(),
                    images: input.images.clone(),
                    response: final_text,
                    tool_calls,
                    ledger,
                };
                if let Err(e) = session::append_turn(cwd, &id, &turn) {
                    println!("session append failed (non-fatal): {e}");
                }
                saved_id = Some(id);
            }
            Err(e) => println!("session store unavailable (non-fatal): {e}"),
        }
    }

    if result.is_ok() && propose_memory {
        maybe_propose_memory(cwd, &config, saved_id.as_deref(), &signals, save_memory).await;
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

fn validate_memory_flags(propose: bool, save: bool) -> Result<(), String> {
    if save && !propose {
        return Err("--save-memory requires --propose-memory".to_string());
    }
    Ok(())
}

/// Detects a memorable moment in the run's turn and drafts one memory.
async fn maybe_propose_memory(
    cwd: &Path,
    config: &KodeConfig,
    session_id: Option<&str>,
    signals: &crate::ledger::TurnSignals,
    save: bool,
) {
    let Some(id) = session_id else {
        return;
    };
    let Ok((turns, _)) = session::load(cwd, id) else {
        return;
    };
    let Some((turn, earlier)) = turns.split_last() else {
        return;
    };
    let Some(trigger) = crate::memory_proposal::detect(turn, earlier, signals) else {
        return;
    };
    let Ok(model) = crate::pipeline::ModelFactory::create(config) else {
        return;
    };
    let runtime = crate::session_runtime::SessionRuntime::new();
    let memory = runtime.memory(&config.ingat).await.ok().flatten();
    let repository = cwd.file_name().map(|n| n.to_string_lossy().to_string());
    let Some(proposal) = crate::memory_proposal::propose(
        model.as_ref(),
        memory.as_deref(),
        repository.clone(),
        &trigger,
        turn,
    )
    .await
    else {
        return;
    };
    eprintln!("◇ remember? \"{}\"", proposal.text);
    if !save {
        return;
    }
    let Some(backend) = memory else {
        eprintln!("memory not saved: memory is disabled in config");
        return;
    };
    let new_memory = crate::remember::proposed_memory(&proposal, repository, false);
    match crate::remember::save_memory(cwd, backend.as_ref(), &new_memory).await {
        Ok(saved) => {
            eprintln!("m ● saved ({saved})");
            let entry = crate::ledger::LedgerEntry::Memory {
                id: saved,
                text: proposal.text,
                team: false,
            };
            let _ = session::amend_last_turn(cwd, id, |t| t.ledger.push(entry));
        }
        Err(e) => eprintln!("memory not saved: {e}"),
    }
}

/// Session that receives this exec run's turn: the latest one with
/// `--continue` (if any exists), otherwise a new one. Every exec run is
/// persisted so `kode receipt` works after it.
fn session_for_run(
    cwd: &Path,
    continue_session: bool,
    provider: &str,
    model: &str,
) -> std::io::Result<String> {
    match continue_session.then(|| session::latest(cwd)).flatten() {
        Some(id) => Ok(id),
        None => session::create(cwd, provider, model),
    }
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

    #[test]
    fn save_memory_without_propose_is_rejected() {
        assert_eq!(
            validate_memory_flags(false, true).unwrap_err(),
            "--save-memory requires --propose-memory"
        );
        assert!(validate_memory_flags(true, false).is_ok());
    }

    #[test]
    fn session_for_run_creates_fresh_without_continue_and_reuses_latest_with_it() {
        let dir = temp_dir("session-for-run");
        let first = session_for_run(&dir, false, "codex", "m").unwrap();
        crate::session::append_turn(
            &dir,
            &first,
            &crate::session::Turn {
                ts: "t".into(),
                task: "x".into(),
                images: vec![],
                response: "y".into(),
                tool_calls: 0,
                ledger: vec![],
            },
        )
        .unwrap();
        assert_eq!(session_for_run(&dir, true, "codex", "m").unwrap(), first);
        assert_ne!(session_for_run(&dir, false, "codex", "m").unwrap(), first);
    }
}
