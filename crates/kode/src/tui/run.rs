use std::io::Stdout;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event, EventStream, KeyCode, KeyEventKind,
    KeyModifiers, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use futures::StreamExt;
use kode_context::git::RepoState;
use kode_core::CancellationToken;
use kode_core::config::{KodeConfig, PermissionMode};
use kode_core::event::{EventBus, KodeEvent};
use kode_memory::EngineeringMemory;
use kode_tools::permission::PermissionHandler;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::{mpsc, oneshot};

use super::commands::*;
use super::draw::{aperture_should_collapse, draw};
use super::events::apply_event;
use super::state::*;
use crate::custom_commands;
use crate::pipeline;
use crate::team_memory;

/// Sends permission requests from tool execution to the UI loop, then awaits
/// the user's y/n answer over a one-shot channel. `auto` is shared with the
/// UI's Shift+Tab auto-mode toggle: when set, `confirm` returns `true`
/// immediately without ever queuing a prompt.
pub struct TuiPermission {
    tx: mpsc::UnboundedSender<(String, oneshot::Sender<bool>)>,
    auto: Arc<AtomicBool>,
}

impl TuiPermission {
    pub fn new(
        tx: mpsc::UnboundedSender<(String, oneshot::Sender<bool>)>,
        auto: Arc<AtomicBool>,
    ) -> Self {
        Self { tx, auto }
    }
}

#[async_trait::async_trait]
impl PermissionHandler for TuiPermission {
    async fn confirm(&self, summary: &str) -> bool {
        if self.auto.load(Ordering::Relaxed) {
            return true;
        }
        let (resp_tx, resp_rx) = oneshot::channel();
        if self.tx.send((summary.to_string(), resp_tx)).is_err() {
            return false;
        }
        resp_rx.await.unwrap_or(false)
    }
}

/// Restores the terminal to its normal mode on drop, including on panic
/// unwind.
pub(crate) struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(std::io::stdout(), DisableMouseCapture, LeaveAlternateScreen);
    }
}

/// Best-effort current branch via `git branch --show-current`. `None` when
/// not a git repo, git is unavailable, or the repo has no commits yet.
pub(crate) fn detect_branch(cwd: &Path) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["branch", "--show-current"])
        .current_dir(cwd)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let branch = String::from_utf8(output.stdout).ok()?;
    let trimmed = branch.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

/// Spawns a non-blocking `git status`/`git diff --numstat` poll
/// (`kode_context::git::repo_state`), sending the result back over `tx`.
/// Called lazily — TUI start and after each task completes — never on a
/// fixed interval, per the dirty-indicator/CURRENT CHANGE design.
pub(crate) fn spawn_git_poll(cwd: std::path::PathBuf, tx: mpsc::UnboundedSender<RepoState>) {
    tokio::spawn(async move {
        if let Some(repo) = kode_context::git::repo_state(&cwd).await {
            let _ = tx.send(repo);
        }
    });
}

/// Starts `task` running through the pipeline: resets per-run state, pushes
/// the user transcript line, and spawns the task future. Shared by the
/// plain-text Enter path and expanded custom-slash-command prompts so both
/// go through the exact same pipeline invocation — returns the child
/// cancellation token to track as `current_cancel`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn submit_task(
    state: &mut AppState,
    cwd: &Path,
    config: &KodeConfig,
    cancel: &CancellationToken,
    events: &EventBus,
    handler: &Arc<dyn PermissionHandler>,
    task: String,
) -> CancellationToken {
    let plan_mode = state.plan_mode;
    state.start_new_task(&task, plan_mode);
    state
        .transcript
        .push(TranscriptLine::new(Gutter::User, task.clone()));
    let child = cancel.child_token();
    state.running = true;
    state.status.state = RunState::Thinking;

    let task_events = events.clone();
    let task_cwd = cwd.to_path_buf();
    let mut task_config = config.clone();
    if state.auto_mode {
        // Belt and braces alongside TuiPermission's `auto` flag: skip the
        // Ask path entirely for runs started while auto mode is on.
        task_config.permissions.default_mode = PermissionMode::Allow;
    }
    let task_handler = handler.clone();
    let task_history: Vec<kode_agent::HistoryTurn> = state
        .history
        .iter()
        .map(|t| kode_agent::HistoryTurn {
            task: t.task.clone(),
            response: t.response.clone(),
        })
        .collect();
    let task_child = child.clone();
    tokio::spawn(async move {
        if let Err(err) = pipeline::run_task(
            &task,
            &task_cwd,
            &task_config,
            task_events.clone(),
            task_handler,
            task_child,
            &task_history,
            plan_mode,
        )
        .await
        {
            task_events.emit(KodeEvent::AgentError {
                message: err.to_string(),
            });
        }
    });
    child
}

/// Launches the interactive TUI. Runs until the user quits (Ctrl-C/'q' while
/// idle) or the process is otherwise terminated. `continue_` resumes the
/// latest session: transcript replayed, history armed for the model.
pub async fn run(cwd: &Path, cancel: CancellationToken, continue_: bool) -> anyhow::Result<()> {
    let mut config = KodeConfig::load(cwd).unwrap_or_default();

    enable_raw_mode()?;
    execute!(std::io::stdout(), EnterAlternateScreen, EnableMouseCapture)?;
    let _guard = TerminalGuard;

    let backend = CrosstermBackend::new(std::io::stdout());
    let mut terminal = Terminal::new(backend)?;

    let mut state = AppState::new(
        config.model.provider.clone(),
        config.model.model.clone(),
        config.model.effort.clone(),
    );
    state.repo_dir = cwd
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| cwd.display().to_string());
    state.branch = detect_branch(cwd);
    state.zindeks_enabled = config.zindeks.enabled;
    state.ingat_enabled = config.ingat.enabled;
    state.reduced_motion = config.ui.reduced_motion;

    if config.ingat.enabled {
        let adapter = kode_memory::IngatAdapter::new(&config.ingat);
        if tokio::time::timeout(Duration::from_secs(3), adapter.health())
            .await
            .is_ok_and(|r| r.is_ok())
        {
            let summary = team_memory::import_on_start(&adapter, cwd).await;
            if let Some(text) = summary.note() {
                state
                    .transcript
                    .push(TranscriptLine::new(Gutter::Ingat, text));
            }
        }
    }

    if let Some(hint) = startup_hint(
        &config.model.provider,
        !config.model.model.is_empty(),
        openai_env_key_present(),
        codex_auth_exists(),
        !opencode_key_ids().is_empty(),
    ) {
        state
            .transcript
            .push(TranscriptLine::new(Gutter::Note, hint));
    }

    if continue_ {
        match crate::session::latest(cwd) {
            Some(id) => {
                restore_session(&mut state, cwd, &id);
            }
            None => state.transcript.push(TranscriptLine::new(
                Gutter::Note,
                "no previous session — starting fresh",
            )),
        }
    }

    let (perm_tx, mut perm_rx) = mpsc::unbounded_channel::<(String, oneshot::Sender<bool>)>();
    let handler: Arc<dyn PermissionHandler> =
        Arc::new(TuiPermission::new(perm_tx, state.auto_flag.clone()));

    let (picker_tx, mut picker_rx) = mpsc::unbounded_channel::<PickerLoaded>();

    let (git_tx, mut git_rx) = mpsc::unbounded_channel::<RepoState>();
    spawn_git_poll(cwd.to_path_buf(), git_tx.clone());

    let events = EventBus::new(256);
    let mut event_rx = events.subscribe();

    let mut key_events = EventStream::new();
    let mut current_cancel: Option<CancellationToken> = None;
    let mut aperture_tick = tokio::time::interval(Duration::from_millis(100));
    // Tracks whether the terminal currently has mouse capture enabled, so
    // Ctrl+T (`state.select_mode`) is synced to the real terminal mode at
    // most once per toggle rather than issuing the escape sequence every
    // frame.
    let mut mouse_captured = true;

    terminal.draw(|f| draw(f, &mut state, cwd))?;

    'outer: loop {
        tokio::select! {
            biased;

            maybe_key = key_events.next() => {
                match maybe_key {
                    Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                        if state.picker.open {
                            match handle_picker_key(&mut state, key.code) {
                                PickerOutcome::Select(selected) => {
                                    state.picker.open = false;
                                    match state.picker.kind {
                                        PickerKind::Model => {
                                            let model = selected;
                                            state.status.model = model.clone();
                                            config.model.model = model.clone();
                                            let _ = KodeConfig::update_model_selection(cwd, Some(&model), None);
                                            state.transcript.push(TranscriptLine::new(
                                                Gutter::Note,
                                                format!("model set: {model}"),
                                            ));
                                        }
                                        PickerKind::Provider => {
                                            let name = selected
                                                .split_whitespace()
                                                .next()
                                                .unwrap_or("")
                                                .to_string();
                                            if VALID_PROVIDERS.contains(&name.as_str()) {
                                                apply_provider_selection(&mut state, cwd, &mut config, &picker_tx, &name);
                                            } else {
                                                state.transcript.push(TranscriptLine::new(
                                                    Gutter::Note,
                                                    format!(
                                                        "invalid provider '{name}' (valid: {})",
                                                        VALID_PROVIDERS.join(", ")
                                                    ),
                                                ));
                                            }
                                        }
                                        PickerKind::Session => {
                                            let id = session_id_from_row(&selected);
                                            restore_session(&mut state, cwd, &id);
                                        }
                                    }
                                }
                                PickerOutcome::Cancel => {
                                    state.picker.open = false;
                                }
                                PickerOutcome::Continue => {}
                            }
                        } else {
                            if handle_key(&mut state, cwd, key.code, key.modifiers, &current_cancel) {
                                break 'outer;
                            }
                            if key.code == KeyCode::Enter && !state.running && !state.input.trim().is_empty() {
                                let mut input = std::mem::take(&mut state.input);
                                let custom = custom_commands::discover(cwd, BUILTIN_COMMAND_NAMES);
                                let hints = slash_hint_items(&input, &custom);
                                if !hints.is_empty() {
                                    // Enter on a hint row completes to the highlighted command.
                                    input = hints[state.slash_selected.min(hints.len() - 1)].0.clone();
                                    state.slash_selected = 0;
                                }
                                if let Some(cmd) = parse_slash_command(&input) {
                                    if let Some(expanded) =
                                        handle_slash_command(&mut state, cwd, &mut config, &picker_tx, cmd)
                                    {
                                        current_cancel = Some(submit_task(
                                            &mut state, cwd, &config, &cancel, &events, &handler, expanded,
                                        ));
                                    }
                                } else if state.status.model.is_empty() {
                                    state.transcript.push(TranscriptLine::new(Gutter::Note, "pick a model first"));
                                    open_picker(&mut state, config.model.provider.clone(), &picker_tx);
                                } else {
                                    let task = input;
                                    current_cancel = Some(submit_task(
                                        &mut state, cwd, &config, &cancel, &events, &handler, task,
                                    ));
                                }
                            }
                        }
                    }
                    Some(Ok(Event::Mouse(mouse))) => {
                        if !state.picker.open {
                            handle_mouse(&mut state, mouse);
                        }
                    }
                    Some(Ok(_)) => {}
                    Some(Err(_)) | None => break 'outer,
                }
            }

            ev = event_rx.recv() => {
                match ev {
                    Ok(ev) => {
                        let finished_tool_calls = match &ev {
                            KodeEvent::TaskFinished { tool_calls, .. } => Some(*tool_calls),
                            _ => None,
                        };
                        let is_agent_error = matches!(ev, KodeEvent::AgentError { .. });
                        apply_event(&mut state, ev);
                        if let Some(tool_calls) = finished_tool_calls {
                            record_completed_turn(
                                &mut state,
                                cwd,
                                &config.model.provider,
                                &config.model.model,
                                tool_calls,
                            );
                            // Refresh the dirty flag + CURRENT CHANGE rows now
                            // that the task's edits (if any) have landed —
                            // same lazy poll as TUI start, no fixed interval.
                            spawn_git_poll(cwd.to_path_buf(), git_tx.clone());
                        } else if is_agent_error {
                            state.pending_task = None;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        apply_event(&mut state, KodeEvent::Note {
                            text: format!("event stream lagged — {n} events dropped"),
                        });
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {}
                }
            }

            perm = perm_rx.recv() => {
                if let Some((summary, responder)) = perm {
                    state.push_permission(PermReq { summary, responder });
                }
            }

            loaded = picker_rx.recv() => {
                if let Some(loaded) = loaded {
                    state.picker.items = loaded.items;
                    state.picker.note = loaded.error;
                    state.picker.selected = 0;
                }
            }

            repo = git_rx.recv() => {
                if let Some(repo) = repo {
                    apply_repo_state(&mut state, repo);
                }
            }

            _ = aperture_tick.tick() => {
                // Also the render-tick clock for the knowledge-band
                // dim→normal fade (item 3) — a new evidence row is dim for
                // its first 2 of these ~100ms ticks.
                state.render_tick = state.render_tick.wrapping_add(1);
                if let Some(ap) = &state.aperture
                    && aperture_should_collapse(ap.received_at, Instant::now(), ap.trigger_seen)
                {
                    state.aperture = None;
                }
            }
        }

        // Sync real terminal mouse capture to `state.select_mode` (Ctrl+T)
        // whenever they disagree — see `mouse_captured`'s doc comment.
        if state.select_mode == mouse_captured {
            if state.select_mode {
                let _ = execute!(std::io::stdout(), DisableMouseCapture);
                mouse_captured = false;
            } else {
                let _ = execute!(std::io::stdout(), EnableMouseCapture);
                mouse_captured = true;
            }
        }

        terminal.draw(|f| draw(f, &mut state, cwd))?;
    }

    if let Some(child) = current_cancel {
        child.cancel();
    }
    cancel.cancel();

    Ok(())
}

/// Handles one key press. Returns `true` if the app should quit.
pub(crate) fn handle_key(
    state: &mut AppState,
    cwd: &Path,
    code: KeyCode,
    modifiers: KeyModifiers,
    current_cancel: &Option<CancellationToken>,
) -> bool {
    if modifiers.contains(KeyModifiers::CONTROL) && code == KeyCode::Char('c') {
        if !state.running {
            return true;
        }
        if let Some(c) = current_cancel {
            c.cancel();
        }
        return false;
    }

    if modifiers.contains(KeyModifiers::CONTROL) && code == KeyCode::Char('k') {
        state.knowledge_band_open = !state.knowledge_band_open;
        return false;
    }

    if modifiers.contains(KeyModifiers::CONTROL) && code == KeyCode::Char('l') {
        state.ledger_open = !state.ledger_open;
        return false;
    }

    if modifiers.contains(KeyModifiers::CONTROL) && code == KeyCode::Char('y') {
        perform_copy(state);
        return false;
    }

    if modifiers.contains(KeyModifiers::CONTROL) && code == KeyCode::Char('t') {
        // Indicator is drawn on the input line (`draw_input`), not pushed
        // into the transcript — it's a mode, not an event.
        state.select_mode = !state.select_mode;
        return false;
    }

    if code == KeyCode::BackTab {
        toggle_auto_mode(state);
        return false;
    }

    // Gate the discovery scan on `/`-prefixed input — cheap for normal
    // typing, and `slash_hint_items` would return empty for anything else
    // anyway.
    let hint_count =
        if state.pending.is_empty() && !state.picker.open && state.input.starts_with('/') {
            let custom = custom_commands::discover(cwd, BUILTIN_COMMAND_NAMES);
            slash_hint_items(&state.input, &custom).len()
        } else {
            0
        };

    match code {
        KeyCode::Esc => {
            if hint_count > 0 {
                state.input.clear();
                state.slash_selected = 0;
            } else if state.ledger_open {
                state.ledger_open = false;
            } else if state.running
                && let Some(c) = current_cancel
            {
                c.cancel();
            }
        }
        KeyCode::Char('q') if !state.running && state.input.is_empty() => {
            return true;
        }
        KeyCode::Char('y') if !state.pending.is_empty() => {
            if let Some(req) = state.pop_permission() {
                let _ = req.responder.send(true);
            }
        }
        KeyCode::Char('n') if !state.pending.is_empty() => {
            if let Some(req) = state.pop_permission() {
                let _ = req.responder.send(false);
            }
        }
        KeyCode::Tab if hint_count > 0 => {
            let custom = custom_commands::discover(cwd, BUILTIN_COMMAND_NAMES);
            let items = slash_hint_items(&state.input, &custom);
            let (name, _) = items[state.slash_selected.min(items.len() - 1)].clone();
            state.input = format!("{name} ");
            state.slash_selected = 0;
        }
        KeyCode::Char(c) => {
            state.input.push(c);
            state.slash_selected = 0;
        }
        KeyCode::Backspace => {
            state.input.pop();
            state.slash_selected = 0;
        }
        KeyCode::Up => {
            if hint_count > 0 {
                state.slash_selected = state.slash_selected.saturating_sub(1);
            } else {
                state.scroll = state.scroll.saturating_sub(1);
                state.follow = false;
            }
        }
        KeyCode::Down => {
            if hint_count > 0 {
                state.slash_selected = (state.slash_selected + 1).min(hint_count - 1);
            } else {
                state.scroll = state.scroll.saturating_add(1);
            }
        }
        KeyCode::PageUp => {
            state.scroll = state.scroll.saturating_sub(10);
            state.follow = false;
        }
        KeyCode::PageDown => {
            state.scroll = state.scroll.saturating_add(10);
        }
        _ => {}
    }
    false
}

/// Maps a mouse wheel event to a transcript scroll delta in lines: `-3` for
/// wheel-up, `+3` for wheel-down. Every other mouse event kind maps to `0`
/// — `handle_mouse` handles left-click separately (toggles a tool-group
/// header under the cursor), drags/moves stay no-ops.
pub(crate) fn wheel_delta(kind: MouseEventKind) -> i32 {
    match kind {
        MouseEventKind::ScrollUp => -3,
        MouseEventKind::ScrollDown => 3,
        _ => 0,
    }
}

/// Handles a mouse event: wheel scrolls the transcript by `wheel_delta`'s
/// 3-line step through the same unclamped-here, clamped-at-render path as
/// `handle_key`'s arrow keys, with the same follow-mode semantics —
/// wheel-up breaks follow (mirrors `KeyCode::Up`), wheel-down leaves it
/// alone (mirrors `KeyCode::Down`). A left-click landing inside the last-
/// rendered transcript area (`state.transcript_hit`, rebuilt every frame by
/// `draw()`) toggles the `expanded` flag of the tool-group header under the
/// cursor, if any — see `hit_test_row`. Everything else is a no-op. Mouse
/// events only arrive at all while capture is enabled (Ctrl+T/select mode
/// releases capture to the terminal for native text selection).
pub(crate) fn handle_mouse(state: &mut AppState, mouse: crossterm::event::MouseEvent) {
    match wheel_delta(mouse.kind) {
        0 => {}
        d if d < 0 => {
            state.scroll = state.scroll.saturating_sub(d.unsigned_abs() as u16);
            state.follow = false;
            return;
        }
        d => {
            state.scroll = state.scroll.saturating_add(d as u16);
            return;
        }
    }

    if let MouseEventKind::Down(crossterm::event::MouseButton::Left) = mouse.kind
        && let Some(hit) = &state.transcript_hit
    {
        let area = hit.area;
        let inside = mouse.column >= area.x
            && mouse.column < area.x + area.width
            && mouse.row >= area.y
            && mouse.row < area.y + area.height;
        if inside {
            let content_row = (mouse.row - area.y) + hit.scroll;
            if let Some(idx) = super::draw::hit_test_row(&hit.rows, content_row)
                && let Some(line) = state.transcript.get_mut(idx)
            {
                line.expanded = !line.expanded;
            }
        }
    }
}

/// Toggles auto mode (Shift+Tab): flips both the UI-visible `auto_mode`
/// bool and the `auto_flag` the permission handler reads from the task
/// task, and leaves a transcript Note describing the new state.
pub(crate) fn toggle_auto_mode(state: &mut AppState) {
    state.auto_mode = !state.auto_mode;
    state.auto_flag.store(state.auto_mode, Ordering::Relaxed);
    let text = if state.auto_mode {
        "auto mode on — tools run without asking"
    } else {
        "auto mode off"
    };
    state
        .transcript
        .push(TranscriptLine::new(Gutter::Note, text));
}

/// Copies `state.last_response` to the OS clipboard (Ctrl+Y / `/copy`) and
/// leaves a transcript Note describing the outcome. Content is never
/// logged — only the char count.
pub(crate) fn perform_copy(state: &mut AppState) {
    if state.last_response.is_empty() {
        state
            .transcript
            .push(TranscriptLine::new(Gutter::Note, "nothing to copy yet"));
        return;
    }
    let note = match copy_to_clipboard(&state.last_response) {
        Ok(n) => format!("copied {n} chars"),
        Err(()) => "no clipboard tool found".to_string(),
    };
    state
        .transcript
        .push(TranscriptLine::new(Gutter::Note, note));
}

/// Pipes `text` to stdin of a platform clipboard tool, trying each
/// candidate in order until one spawns and exits successfully. Windows:
/// `clip`. macOS: `pbcopy`. Linux: `wl-copy` then `xclip -selection
/// clipboard`. Returns the copied char count, or `Err(())` when no
/// candidate tool is available/working.
pub(crate) fn copy_to_clipboard(text: &str) -> Result<usize, ()> {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let candidates: &[(&str, &[&str])] = if cfg!(target_os = "windows") {
        &[("clip", &[])]
    } else if cfg!(target_os = "macos") {
        &[("pbcopy", &[])]
    } else {
        &[("wl-copy", &[]), ("xclip", &["-selection", "clipboard"])]
    };

    for (cmd, args) in candidates {
        let Ok(mut child) = Command::new(cmd)
            .args(*args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            continue;
        };
        let Some(mut stdin) = child.stdin.take() else {
            continue;
        };
        if stdin.write_all(text.as_bytes()).is_err() {
            continue;
        }
        drop(stdin);
        if child.wait().map(|s| s.success()).unwrap_or(false) {
            return Ok(text.chars().count());
        }
    }
    Err(())
}

#[allow(dead_code)]
pub(crate) type Backend = CrosstermBackend<Stdout>;
