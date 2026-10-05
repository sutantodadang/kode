use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::future::Future;
use std::io::{self, Stdout, Write};
use std::panic::AssertUnwindSafe;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event,
    EventStream, KeyCode, KeyEventKind, KeyModifiers, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
#[cfg(any(windows, test))]
use futures::Stream;
use futures::{FutureExt, StreamExt};
use kode_context::git::RepoState;
use kode_core::config::{KodeConfig, PermissionMode};
use kode_core::event::{EventBus, KodeEvent};
use kode_core::{CancellationToken, UserInput};
use kode_tools::permission::PermissionHandler;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::{mpsc, oneshot};

use super::commands::*;
use super::draw::{draw, line_animating};
use super::events::{apply_event, flush_model_stream};
use super::state::*;
use super::theme;
use crate::custom_commands;
use crate::first_run::SetupCard;
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
        let _ = execute!(
            std::io::stdout(),
            DisableBracketedPaste,
            DisableMouseCapture,
            LeaveAlternateScreen
        );
    }
}

/// Native engines write diagnostics to the process stderr. Keep those bytes
/// in a file while the alternate screen is active so they cannot overwrite
/// the composer or be mistaken for user input.
struct TuiStderrGuard {
    #[cfg(windows)]
    original: *mut std::ffi::c_void,
    #[cfg(unix)]
    original: i32,
    _file: File,
}

impl TuiStderrGuard {
    fn redirect() -> io::Result<Self> {
        let root = kode_core::kode_home_dir().unwrap_or_else(std::env::temp_dir);
        let dir = root.join("logs");
        std::fs::create_dir_all(&dir)?;
        let mut options = OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(dir.join("tui-stderr.log"))?;
        io::stderr().flush()?;

        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            unsafe extern "system" {
                fn GetStdHandle(kind: u32) -> *mut std::ffi::c_void;
                fn SetStdHandle(kind: u32, handle: *mut std::ffi::c_void) -> i32;
            }
            const STDERR_HANDLE: u32 = -12_i32 as u32;
            let original = unsafe { GetStdHandle(STDERR_HANDLE) };
            if original.is_null() || original as isize == -1 {
                return Err(io::Error::last_os_error());
            }
            if unsafe { SetStdHandle(STDERR_HANDLE, file.as_raw_handle()) } == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(Self {
                original,
                _file: file,
            })
        }

        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            unsafe extern "C" {
                fn dup(fd: i32) -> i32;
                fn dup2(from: i32, to: i32) -> i32;
                fn close(fd: i32) -> i32;
            }
            let original = unsafe { dup(2) };
            if original < 0 {
                return Err(io::Error::last_os_error());
            }
            if unsafe { dup2(file.as_raw_fd(), 2) } < 0 {
                let error = io::Error::last_os_error();
                unsafe { close(original) };
                return Err(error);
            }
            Ok(Self {
                original,
                _file: file,
            })
        }
    }
}

impl Drop for TuiStderrGuard {
    fn drop(&mut self) {
        #[cfg(windows)]
        {
            unsafe extern "system" {
                fn SetStdHandle(kind: u32, handle: *mut std::ffi::c_void) -> i32;
            }
            unsafe { SetStdHandle(-12_i32 as u32, self.original) };
        }
        #[cfg(unix)]
        {
            unsafe extern "C" {
                fn dup2(from: i32, to: i32) -> i32;
                fn close(fd: i32) -> i32;
            }
            unsafe {
                dup2(self.original, 2);
                close(self.original);
            }
        }
    }
}

pub(crate) fn append_paste_at(state: &mut AppState, cwd: &Path, pasted: &str) {
    if !state.pending.is_empty() {
        return;
    }
    state.begin_composing();
    if !pasted.contains(['\r', '\n']) && crate::attachments::looks_like_image_path(pasted) {
        if let Err(err) = state.add_image_path(cwd, pasted) {
            state.transcript.push(TranscriptLine::new(
                Gutter::Note,
                format!("image attachment failed: {err}"),
            ));
        }
        return;
    }
    state.add_paste(pasted);
}

#[cfg(test)]
pub(crate) fn append_paste(state: &mut AppState, pasted: &str) {
    let cwd = std::env::current_dir().unwrap_or_default();
    append_paste_at(state, &cwd, pasted);
}

#[cfg(any(windows, test))]
pub(crate) const WINDOWS_PASTE_PROBE: Duration = Duration::from_millis(3);
#[cfg(any(windows, test))]
pub(crate) const WINDOWS_PASTE_IDLE: Duration = Duration::from_millis(12);

/// Crossterm's native Windows input backend exposes clipboard paste as a burst
/// of ordinary key events, including `Enter` for every newline. Collecting the
/// burst before dispatch keeps those newlines from submitting partial prompts.
async fn next_terminal_event(
    events: &mut EventStream,
    pending: &mut VecDeque<Event>,
) -> Option<std::io::Result<Event>> {
    if let Some(event) = pending.pop_front() {
        return Some(Ok(event));
    }

    let first = events.next().await?;

    #[cfg(not(windows))]
    {
        Some(first)
    }

    #[cfg(windows)]
    {
        let first = match first {
            Ok(event) => event,
            Err(error) => return Some(Err(error)),
        };
        Some(coalesce_windows_terminal_event(first, events, pending).await)
    }
}

#[cfg(any(windows, test))]
fn can_be_windows_paste_key(event: &Event) -> bool {
    matches!(
        event,
        Event::Key(key)
            if key.kind == KeyEventKind::Press
                && match key.code {
                    KeyCode::Char(_) | KeyCode::Tab => !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT),
                    KeyCode::Enter => true,
                    _ => false,
                }
    )
}

#[cfg(any(windows, test))]
fn is_windows_key_release(event: &Event) -> bool {
    matches!(event, Event::Key(key) if key.kind == KeyEventKind::Release)
}

/// Probe briefly for a second printable key before paying the full paste idle
/// window. Human typing normally has no second press within this probe, so the
/// first character reaches the composer without the old 12 ms delay. Clipboard
/// injection arrives as queued press events and still enters paste collection.
#[cfg(any(windows, test))]
pub(crate) async fn coalesce_windows_terminal_event<S>(
    first: Event,
    events: &mut S,
    pending: &mut VecDeque<Event>,
) -> std::io::Result<Event>
where
    S: Stream<Item = std::io::Result<Event>> + Unpin,
{
    if !can_be_windows_paste_key(&first) {
        return Ok(first);
    }

    let mut between = Vec::new();
    let second = loop {
        match tokio::time::timeout(WINDOWS_PASTE_PROBE, events.next()).await {
            Ok(Some(Ok(event))) if is_windows_key_release(&event) => between.push(event),
            Ok(Some(Ok(event))) => break Some(event),
            Ok(Some(Err(error))) => return Err(error),
            Ok(None) | Err(_) => break None,
        }
    };
    let Some(second) = second else {
        pending.extend(between);
        return Ok(first);
    };
    if !can_be_windows_paste_key(&second) {
        pending.extend(between);
        pending.push_back(second);
        return Ok(first);
    }

    let mut batch = vec![first];
    batch.extend(between);
    batch.push(second);
    loop {
        match tokio::time::timeout(WINDOWS_PASTE_IDLE, events.next()).await {
            Ok(Some(Ok(event))) => {
                let is_key_event = matches!(&event, Event::Key(_));
                batch.push(event);
                if !is_key_event {
                    break;
                }
            }
            Ok(Some(Err(error))) => return Err(error),
            Ok(None) | Err(_) => break,
        }
    }

    if let Some(pasted) = windows_paste_text(&batch) {
        return Ok(Event::Paste(pasted));
    }

    let first = batch.remove(0);
    pending.extend(batch);
    Ok(first)
}

/// Recover a Windows clipboard burst as text. A real key press may be followed
/// immediately by its release event, so the classifier requires either a
/// multiline burst or at least 32 text key presses. Ctrl/Alt character chords
/// remain normal shortcuts; Ctrl+Enter is accepted because terminals are known
/// to attach Ctrl to pasted newlines while bracketed paste mode is enabled.
#[cfg(any(windows, test))]
pub(crate) fn windows_paste_text(events: &[Event]) -> Option<String> {
    let mut text = String::new();
    let mut text_press_count = 0usize;
    let mut has_newline = false;

    for event in events {
        let Event::Key(key) = event else {
            return None;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }

        match key.code {
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                text.push(character);
                text_press_count += 1;
            }
            KeyCode::Enter => {
                text.push('\n');
                text_press_count += 1;
                has_newline = true;
            }
            KeyCode::Tab
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                text.push('\t');
                text_press_count += 1;
            }
            _ => return None,
        }
    }

    ((has_newline && text_press_count >= 2) || text_press_count >= 32).then_some(text)
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

/// Leaves the alternate screen so a CLI-interactive flow (browser login,
/// download output) can use the real terminal, then restores the TUI —
/// also when the flow fails, so the caller can report the error in the
/// transcript.
async fn with_terminal_suspended<T>(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    stderr_guard: &mut Option<TuiStderrGuard>,
    flow: impl Future<Output = T>,
) -> io::Result<T> {
    disable_raw_mode()?;
    execute!(
        std::io::stdout(),
        DisableBracketedPaste,
        DisableMouseCapture,
        LeaveAlternateScreen
    )?;
    *stderr_guard = None; // restores the real stderr for prompts
    let out = flow.await;
    *stderr_guard = Some(TuiStderrGuard::redirect()?);
    enable_raw_mode()?;
    execute!(
        std::io::stdout(),
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste
    )?;
    terminal.clear()?;
    Ok(out)
}

fn setup_facts(state: &AppState, config: &KodeConfig, cwd: &Path) -> crate::first_run::SetupFacts {
    crate::first_run::SetupFacts {
        model_configured: !config.model.model.is_empty(),
        provider_logged_in: provider_logged_in(&config.model.provider),
        zindeks_enabled: config.zindeks.enabled,
        engine_installed: crate::engine_assets::zindeks_library(&config.zindeks).is_ok(),
        repo_indexed: state.repo_indexed,
        index_prompt: kode_core::kode_home_dir()
            .and_then(|home| crate::first_run::load_index_prompt(&home, cwd)),
    }
}

/// Shows the next applicable setup card when nothing else is on screen.
fn maybe_show_setup_card(state: &mut AppState, config: &KodeConfig, cwd: &Path) {
    if state.picker.open
        || state.running
        || !state.pending.is_empty()
        || state.composer_has_content()
    {
        return;
    }
    if !state.setup_dirty {
        return;
    }
    let facts = setup_facts(state, config, cwd);
    state.setup_dirty = false;
    let Some(card) = crate::first_run::next_card(&facts, &state.setup_skipped) else {
        state.setup_card = None;
        return;
    };
    let mut remaining = state.setup_skipped.clone();
    let mut total = 0;
    while let Some(c) = crate::first_run::next_card(&facts, &remaining) {
        remaining.insert(c);
        total += 1;
    }
    let position = state.setup_skipped.len() + 1;
    open_setup_card(
        state,
        card,
        &config.model.provider,
        position,
        position + total - 1,
    );
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

/// Detects a memorable moment in the just-finished turn and, only then,
/// drafts one memory on a background task.
fn spawn_memory_proposal(
    state: &AppState,
    config: &KodeConfig,
    cwd: &Path,
    tx: &mpsc::UnboundedSender<crate::memory_proposal::Proposal>,
) {
    if !config.memory.propose {
        return;
    }
    let Some((turn, earlier)) = state.history.split_last() else {
        return;
    };
    let Some(trigger) = crate::memory_proposal::detect(turn, earlier, &state.last_signals) else {
        return; // no trigger: no request, no tokens
    };
    let Ok(model) = crate::pipeline::ModelFactory::create(config) else {
        return;
    };
    let turn = turn.clone();
    let runtime = state.runtime.clone();
    let ingat = config.ingat.clone();
    let repository = cwd.file_name().map(|n| n.to_string_lossy().to_string());
    let tx = tx.clone();
    tokio::spawn(async move {
        let memory = runtime.memory(&ingat).await.ok().flatten();
        if let Some(p) = crate::memory_proposal::propose(
            model.as_ref(),
            memory.as_deref(),
            repository,
            &trigger,
            &turn,
        )
        .await
        {
            let _ = tx.send(p);
        }
    });
}

/// A memory that finished saving.
struct SavedMemory {
    id: String,
    text: String,
    team: bool,
    /// `state.history` index the memory belongs to; `None` for `/remember`,
    /// which is not part of any turn.
    turn: Option<usize>,
}

/// Saves an approved proposal as a personal or team memory.
fn spawn_save_memory(
    state: &AppState,
    config: &KodeConfig,
    cwd: &Path,
    tx: &mpsc::UnboundedSender<Result<SavedMemory, String>>,
    proposal: crate::memory_proposal::Proposal,
    team: bool,
    turn: usize,
) {
    let runtime = state.runtime.clone();
    let ingat = config.ingat.clone();
    let repository = cwd.file_name().map(|n| n.to_string_lossy().to_string());
    let root = cwd.to_path_buf();
    let tx = tx.clone();
    tokio::spawn(async move {
        let memory = crate::remember::proposed_memory(&proposal, repository, team);
        let text = memory.body.clone();
        let result = match runtime.memory(&ingat).await {
            Ok(Some(backend)) => crate::remember::save_memory(&root, backend.as_ref(), &memory)
                .await
                .map(|id| SavedMemory {
                    id,
                    text,
                    team,
                    turn: Some(turn),
                })
                .map_err(|e| e.to_string()),
            Ok(None) => Err("memory is disabled in config".to_string()),
            Err(e) => Err(e.to_string()),
        };
        let _ = tx.send(result);
    });
}

/// Saves a `/remember [--team] <text>` memory directly.
fn spawn_save_command_memory(
    state: &AppState,
    config: &KodeConfig,
    cwd: &Path,
    tx: &mpsc::UnboundedSender<Result<SavedMemory, String>>,
    text: String,
    team: bool,
) {
    use kode_memory::{MemoryContext, MemoryKind, NewMemory, Provenance};

    let runtime = state.runtime.clone();
    let ingat = config.ingat.clone();
    let repository = cwd.file_name().map(|n| n.to_string_lossy().to_string());
    let root = cwd.to_path_buf();
    let tx = tx.clone();
    tokio::spawn(async move {
        let memory = NewMemory {
            kind: MemoryKind::Convention,
            summary: text.chars().take(100).collect(),
            body: text.clone(),
            tags: vec![],
            provenance: Provenance::ExplicitUser,
            context: MemoryContext {
                repository,
                ..Default::default()
            },
            team,
        };
        let result = match runtime.memory(&ingat).await {
            Ok(Some(backend)) => crate::remember::save_memory(&root, backend.as_ref(), &memory)
                .await
                .map(|id| SavedMemory {
                    id,
                    text,
                    team,
                    turn: None,
                })
                .map_err(|e| e.to_string()),
            Ok(None) => Err("memory is disabled in config".to_string()),
            Err(e) => Err(e.to_string()),
        };
        let _ = tx.send(result);
    });
}

/// Zero-token repo tour: map, start points, grouped team memory.
fn spawn_onboard(state: &AppState, config: &KodeConfig, cwd: &Path, events: &EventBus) {
    let runtime = state.runtime.clone();
    let cfg = config.zindeks.clone();
    let root = cwd.to_path_buf();
    let events = events.clone();
    tokio::spawn(async move {
        for ev in crate::onboard::onboard_events(&runtime, &cfg, &root).await {
            events.emit(ev);
        }
    });
}

/// Renders the repo map into the transcript via the shared event bus.
fn spawn_repo_map(state: &AppState, config: &KodeConfig, cwd: &Path, events: &EventBus) {
    let runtime = state.runtime.clone();
    let cfg = config.zindeks.clone();
    let root = cwd.to_path_buf();
    let events = events.clone();
    tokio::spawn(async move {
        for ev in crate::repo_map::map_events(&runtime, &cfg, &root).await {
            events.emit(ev);
        }
    });
}

/// Indexes the repo on a dedicated engine (so running tasks are never
/// queued behind it), then drops the session's cached engine so the next
/// task binds the fresh index, and shows the repo map.
fn spawn_background_index(
    state: &mut AppState,
    config: &KodeConfig,
    cwd: &Path,
    events: &EventBus,
) {
    if state.indexing_since.is_some() {
        state.transcript.push(TranscriptLine::new(
            Gutter::Note,
            "indexing is already running",
        ));
        return;
    }
    if !config.zindeks.enabled {
        state.transcript.push(TranscriptLine::new(
            Gutter::Note,
            "zindeks is disabled in config — enable [zindeks] to index",
        ));
        return;
    }
    events.emit(KodeEvent::IndexStarted);
    let runtime = state.runtime.clone();
    let cfg = config.zindeks.clone();
    let root = cwd.to_path_buf();
    let events = events.clone();
    tokio::spawn(async move {
        let started = Instant::now();
        let result: anyhow::Result<Option<u64>> = async {
            let backend = crate::intel_backend::connect(&cfg, &root)
                .await?
                .ok_or_else(|| anyhow::anyhow!("no code-intelligence backend is enabled"))?;
            backend.index_repository().await?;
            Ok(backend.health().await.ok().map(|h| h.documents))
        }
        .await;
        runtime.forget_intel().await;
        let elapsed_ms = started.elapsed().as_millis() as u64;
        match result {
            Ok(files) => {
                if let Some(home) = kode_core::kode_home_dir() {
                    let _ = crate::first_run::save_index_prompt(
                        &home,
                        &root,
                        crate::first_run::IndexPrompt::Accepted,
                    );
                }
                events.emit(KodeEvent::IndexFinished {
                    files,
                    error: None,
                    elapsed_ms,
                });
                for ev in crate::repo_map::map_events(&runtime, &cfg, &root).await {
                    events.emit(ev);
                }
            }
            Err(e) => events.emit(KodeEvent::IndexFinished {
                files: None,
                error: Some(e.to_string()),
                elapsed_ms,
            }),
        }
    });
}

/// Starts `task` running through the pipeline: resets per-run state, pushes
/// the user transcript line, and spawns the task future. Shared by the
/// plain-text Enter path and expanded custom-slash-command prompts so both
/// go through the exact same pipeline invocation — returns the child
/// cancellation token and steering sender for the active run.
pub(crate) struct SubmittedTask {
    pub cancel: CancellationToken,
    pub steering: mpsc::UnboundedSender<UserInput>,
}

pub(crate) fn push_user_transcript(state: &mut AppState, message: impl Into<UserInput>) {
    let message = message.into();
    state
        .transcript
        .extend(user_input_transcript_lines(&message));
}

pub(crate) fn route_running_input(
    state: &mut AppState,
    steering_tx: Option<&mpsc::UnboundedSender<UserInput>>,
    queued_followup: &mut Option<UserInput>,
    message: impl Into<UserInput>,
) {
    let message = message.into();
    flush_model_stream(state);
    push_user_transcript(state, &message);
    let sent =
        state.steering_active && steering_tx.is_some_and(|tx| tx.send(message.clone()).is_ok());
    if sent {
        return;
    }

    match queued_followup.as_mut() {
        Some(queued) => queued.append(message),
        None => *queued_followup = Some(message),
    }
    state.transcript.push(TranscriptLine::new(
        Gutter::Note,
        "queued for the next turn after the current run finishes",
    ));
}

async fn guard_task<F, T>(task: F) -> Result<T, String>
where
    F: Future<Output = anyhow::Result<T>>,
{
    match AssertUnwindSafe(task).catch_unwind().await {
        Ok(result) => result.map_err(|error| error.to_string()),
        Err(_) => Err("agent task panicked; run recovered".to_string()),
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn submit_task_with(
    state: &mut AppState,
    cwd: &Path,
    config: &KodeConfig,
    cancel: &CancellationToken,
    events: &EventBus,
    handler: &Arc<dyn PermissionHandler>,
    task: UserInput,
    echo_user: bool,
    allow_graph_answer: bool,
) -> SubmittedTask {
    let plan_mode = state.plan_mode;
    state.start_new_task(task.clone(), plan_mode);
    push_graph_warming_note(state);
    if echo_user {
        push_user_transcript(state, &task);
    }
    let child = cancel.child_token();
    let (steering_tx, steering_rx) = mpsc::unbounded_channel();
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
            images: t.images.clone(),
            response: t.response.clone(),
        })
        .collect();
    let task_child = child.clone();
    let task_cache_key = state.cache_key.clone();
    let task_runtime = state.runtime.clone();
    tokio::spawn(async move {
        if let Err(message) = guard_task(pipeline::run_task_with_input(
            &task,
            &task_cwd,
            &task_config,
            task_events.clone(),
            task_handler,
            task_child,
            &task_history,
            plan_mode,
            Some(steering_rx),
            Some(task_cache_key),
            &task_runtime,
            allow_graph_answer,
        ))
        .await
        {
            task_events.emit(KodeEvent::AgentError { message });
        }
    });
    SubmittedTask {
        cancel: child,
        steering: steering_tx,
    }
}

/// Submits a task with graph answers allowed (the common path).
#[allow(clippy::too_many_arguments)]
pub(crate) fn submit_task(
    state: &mut AppState,
    cwd: &Path,
    config: &KodeConfig,
    cancel: &CancellationToken,
    events: &EventBus,
    handler: &Arc<dyn PermissionHandler>,
    task: UserInput,
    echo_user: bool,
) -> SubmittedTask {
    submit_task_with(
        state, cwd, config, cancel, events, handler, task, echo_user, true,
    )
}

fn record_failed_turn(
    state: &mut AppState,
    cwd: &Path,
    provider: &str,
    model: &str,
    error: &str,
    had_partial_response: bool,
) {
    state.last_response = if had_partial_response {
        format!("{}\n\n[run stopped: {error}]", state.last_response)
    } else {
        format!("[run stopped: {error}]")
    };
    record_completed_turn(state, cwd, provider, model, 0);
}

/// Launches the interactive TUI. Runs until the user quits (Ctrl-C while
/// idle) or the process is otherwise terminated. `continue_` resumes the
/// latest session: transcript replayed, history armed for the model.
pub async fn run(cwd: &Path, cancel: CancellationToken, continue_: bool) -> anyhow::Result<()> {
    let mut config = KodeConfig::load(cwd).unwrap_or_default();

    let mut stderr_guard = Some(TuiStderrGuard::redirect()?);

    enable_raw_mode()?;
    execute!(
        std::io::stdout(),
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste
    )?;
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
    let palette = theme::palette_mode(
        config.ui.theme == kode_core::config::UiTheme::Light,
        std::env::var("COLORTERM").ok().as_deref(),
        std::env::var_os("WT_SESSION").is_some(),
    );

    if let Ok(Some(adapter)) = crate::memory_backend::connect(&config.ingat).await
        && tokio::time::timeout(Duration::from_secs(3), adapter.health())
            .await
            .is_ok_and(|r| r.is_ok())
    {
        let summary = team_memory::import_on_start(adapter.as_ref(), cwd).await;
        if let Some(text) = summary.note() {
            state
                .transcript
                .push(TranscriptLine::new(Gutter::Ingat, text));
        }
    }

    if continue_ {
        match crate::session::latest(cwd) {
            Some(id) => {
                if restore_session(&mut state, cwd, &id) {
                    config.model.provider = state.status.provider.clone();
                    config.model.model = state.status.model.clone();
                }
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

    let (memory_tx, mut memory_rx) = mpsc::unbounded_channel::<crate::memory_proposal::Proposal>();
    let (memory_saved_tx, mut memory_saved_rx) =
        mpsc::unbounded_channel::<Result<SavedMemory, String>>();

    let (git_tx, mut git_rx) = mpsc::unbounded_channel::<RepoState>();
    spawn_git_poll(cwd.to_path_buf(), git_tx.clone());

    let (indexed_tx, mut indexed_rx) = mpsc::unbounded_channel::<Option<bool>>();
    let mut indexed_probe_done = false;
    if config.zindeks.enabled && crate::engine_assets::zindeks_library(&config.zindeks).is_ok() {
        let runtime = state.runtime.clone();
        let cfg = config.zindeks.clone();
        let root = cwd.to_path_buf();
        tokio::spawn(async move {
            let answer = match runtime.intel(&cfg, &root).await {
                Ok(Some(handle)) => match handle.backend.ensure_bound().await {
                    Ok(()) => Some(true),
                    Err(kode_intel::IntelError::NotIndexed(_)) => {
                        // Never keep an unbound engine: the pipeline assumes
                        // a reused handle is bound.
                        runtime.forget_intel().await;
                        Some(false)
                    }
                    Err(_) => {
                        runtime.forget_intel().await;
                        None
                    }
                },
                _ => None,
            };
            let _ = indexed_tx.send(answer);
        });
    }

    let events = EventBus::new(256);
    let mut event_rx = events.subscribe();

    let mut key_events = EventStream::new();
    let mut pending_terminal_events = VecDeque::new();
    let mut current_cancel: Option<CancellationToken> = None;
    let mut current_steering: Option<mpsc::UnboundedSender<UserInput>> = None;
    let mut queued_followup: Option<UserInput> = None;
    let mut ui_tick = tokio::time::interval(Duration::from_millis(100));
    // Kode Benang motion cadence: only polled while something can animate
    // (see `motion_active`); the idle path keeps the 100 ms tick above.
    let mut motion_tick = tokio::time::interval(Duration::from_millis(50));
    motion_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Tracks whether the terminal currently has mouse capture enabled, so
    // Ctrl+T (`state.select_mode`) is synced to the real terminal mode at
    // most once per toggle rather than issuing the escape sequence every
    // frame.
    let mut mouse_captured = true;

    terminal.draw(|f| {
        draw(f, &mut state, cwd);
        theme::adapt(f.buffer_mut(), palette);
    })?;

    // Warm the process-wide Laya cache while the user composes the first
    // task. Drawing/input never waits for model loading.
    if !config.model.model.is_empty()
        && crate::routing::router_active(&config.router, !kode_local::pins::MODEL_FILES.is_empty())
    {
        let router_config = config.router.clone();
        let root = cwd.to_path_buf();
        tokio::spawn(async move {
            let _ = guard_task(async { Ok(crate::local::load(&router_config, &root).await) }).await;
        });
    }

    // Compared at the top of every iteration (not after the select) so a
    // picker closed by a handler that `continue 'outer`s still marks the
    // setup facts dirty.
    let mut picker_was_open = state.picker.open;
    'outer: loop {
        if picker_was_open && !state.picker.open {
            state.setup_dirty = true;
        }
        picker_was_open = state.picker.open;
        let animating = motion_active(&state, Instant::now());
        tokio::select! {
            biased;

            maybe_key = next_terminal_event(&mut key_events, &mut pending_terminal_events) => {
                match maybe_key {
                    Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                        if state.running
                            && state.pending.is_empty()
                            && key.code == KeyCode::Char('c')
                            && key.modifiers.contains(KeyModifiers::CONTROL)
                        {
                            if let Some(cancel) = &current_cancel {
                                cancel.cancel();
                            }
                        } else if state.picker.open {
                            let picker_outcome = if key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) {
                                if key.code == KeyCode::Char('p') && key.modifiers.contains(KeyModifiers::CONTROL) {
                                    PickerOutcome::Cancel
                                } else {
                                    PickerOutcome::Continue
                                }
                            } else {
                                handle_picker_key(&mut state, key.code)
                            };
                            match picker_outcome {
                                PickerOutcome::Select(selected) => {
                                    match state.picker.kind {
                                        PickerKind::Model => {
                                            commit_model_selection(&mut state, cwd, &mut config, &selected);
                                        }
                                        PickerKind::Provider => {
                                            let name = selected
                                                .split_whitespace()
                                                .next()
                                                .unwrap_or("")
                                                .to_string();
                                            if VALID_PROVIDERS.contains(&name.as_str()) {
                                                state.picker.open = false;
                                                apply_provider_selection(&mut state, &picker_tx, &name);
                                            } else {
                                                state.picker.note = Some(format!("invalid provider: {name}"));
                                            }
                                        }
                                        PickerKind::Session => {
                                            state.picker.open = false;
                                            let id = session_id_from_row(&selected);
                                            if restore_session(&mut state, cwd, &id) {
                                                config.model.provider = state.status.provider.clone();
                                                config.model.model = state.status.model.clone();
                                            }
                                        }
                                        PickerKind::Command => {
                                            state.picker.open = false;
                                            if let Some(name) = selected.split_whitespace().next() {
                                                if name == "/exit" {
                                                    break 'outer;
                                                }
                                                let needs_argument = matches!(name, "/effort" | "/image")
                                                    || !BUILTIN_COMMAND_NAMES.contains(&name.trim_start_matches('/'));
                                                if needs_argument {
                                                    if state.input.is_empty() {
                                                        state.input = format!("{name} ");
                                                        state.input_cursor = None;
                                                    } else {
                                                        state.transcript.push(TranscriptLine::new(
                                                            Gutter::Note,
                                                            format!("draft kept; type {name} after sending or clearing it"),
                                                        ));
                                                    }
                                                } else if name == "/map" {
                                                    spawn_repo_map(&state, &config, cwd, &events);
                                                } else if name == "/index" {
                                                    spawn_background_index(&mut state, &config, cwd, &events);
                                                } else if name == "/onboard" {
                                                    spawn_onboard(&state, &config, cwd, &events);
                                                } else if let Some(command) = parse_slash_command(name) {
                                                    handle_slash_command(&mut state, cwd, &mut config, &picker_tx, command);
                                                }
                                            }
                                        }
                                        PickerKind::Setup => {
                                            state.picker.open = false;
                                            match state.setup_card.take() {
                                                Some(SetupCard::Provider) => open_provider_picker(&mut state),
                                                Some(SetupCard::Login) => {
                                                    let provider = config.model.provider.clone();
                                                    let outcome = with_terminal_suspended(
                                                        &mut terminal,
                                                        &mut stderr_guard,
                                                        crate::auth::login(&provider),
                                                    ).await;
                                                    mouse_captured = true;
                                                    state.select_mode = false;
                                                    let text = match outcome {
                                                        Ok(Ok(())) => format!("logged in to {provider}"),
                                                        Ok(Err(e)) => format!("login failed: {e}"),
                                                        Err(e) => format!("terminal restore failed: {e}"),
                                                    };
                                                    state.transcript.push(TranscriptLine::new(Gutter::Note, text));
                                                    state.setup_skipped.insert(SetupCard::Login);
                                                }
                                                Some(SetupCard::Engine) => {
                                                    let cfg = config.zindeks.clone();
                                                    let outcome = with_terminal_suspended(
                                                        &mut terminal,
                                                        &mut stderr_guard,
                                                        crate::setup::install_zindeks(&cfg),
                                                    ).await;
                                                    mouse_captured = true;
                                                    state.select_mode = false;
                                                    let text = match outcome {
                                                        Ok(Ok(())) => "code-graph engine installed".to_string(),
                                                        Ok(Err(e)) => format!("engine download failed: {e}"),
                                                        Err(e) => format!("terminal restore failed: {e}"),
                                                    };
                                                    state.transcript.push(TranscriptLine::new(Gutter::Note, text));
                                                    state.setup_skipped.insert(SetupCard::Engine);
                                                    state.repo_indexed = Some(false); // freshly installed engine has no index
                                                }
                                                Some(SetupCard::Index) => {
                                                    state.setup_skipped.insert(SetupCard::Index);
                                                    spawn_background_index(&mut state, &config, cwd, &events);
                                                }
                                                None => {}
                                            }
                                        }
                                    }
                                }
                                PickerOutcome::Cancel => {
                                    if state.picker.kind == PickerKind::Setup
                                        && let Some(card) = state.setup_card
                                    {
                                        state.setup_skipped.insert(card);
                                        if card == SetupCard::Index
                                            && let Some(home) = kode_core::kode_home_dir()
                                            && let Err(e) = crate::first_run::save_index_prompt(
                                                &home,
                                                cwd,
                                                crate::first_run::IndexPrompt::Declined,
                                            )
                                        {
                                            state.transcript.push(TranscriptLine::new(
                                                Gutter::Note,
                                                format!("could not remember index choice: {e}"),
                                            ));
                                        }
                                    }
                                    state.picker.open = false;
                                }
                                PickerOutcome::Continue => {}
                            }
                        } else {
                            if !state.running
                                && state.graph_offer.is_some()
                                && !state.composer_has_content()
                            {
                                if key.code == KeyCode::Esc {
                                    state.graph_offer = None;
                                    continue 'outer;
                                }
                                if key.code == KeyCode::Enter {
                                    let original = state.graph_offer.take().unwrap();
                                    let input =
                                        ask_model_anyway_input(&original, &state.last_response);
                                    if config.router.training.enabled {
                                        let note = crate::router_cmd::correct(
                                            cwd,
                                            "last",
                                            &["answer=model".to_string()],
                                        )
                                        .unwrap_or_else(|e| e);
                                        state
                                            .transcript
                                            .push(TranscriptLine::new(Gutter::Note, note));
                                    }
                                    let submitted = submit_task_with(
                                        &mut state,
                                        cwd,
                                        &config,
                                        &cancel,
                                        &events,
                                        &handler,
                                        input,
                                        true,
                                        false,
                                    );
                                    current_cancel = Some(submitted.cancel);
                                    current_steering = Some(submitted.steering);
                                    continue 'outer;
                                }
                            }
                            if !state.running
                                && state.memory_offer.is_some()
                                && !state.composer_has_content()
                            {
                                match key.code {
                                    KeyCode::Esc => {
                                        state.memory_offer = None;
                                        continue 'outer;
                                    }
                                    KeyCode::Enter | KeyCode::Tab => {
                                        let proposal = state.memory_offer.take().unwrap();
                                        let team = key.code == KeyCode::Tab;
                                        spawn_save_memory(
                                            &state,
                                            &config,
                                            cwd,
                                            &memory_saved_tx,
                                            proposal,
                                            team,
                                            state.history.len().saturating_sub(1),
                                        );
                                        continue 'outer;
                                    }
                                    KeyCode::Char('e')
                                        if key.modifiers.contains(KeyModifiers::CONTROL) =>
                                    {
                                        let proposal = state.memory_offer.take().unwrap();
                                        state.input = format!("/remember {}", proposal.text);
                                        state.input_cursor = None;
                                        continue 'outer;
                                    }
                                    _ => {}
                                }
                            }
                            if handle_key(&mut state, cwd, key.code, key.modifiers, &current_cancel) {
                                break 'outer;
                            }
                            if key.code == KeyCode::Enter
                                && !key.modifiers.contains(KeyModifiers::SHIFT)
                                && state.composer_has_content()
                            {
                                if state.running {
                                    let steering = state.take_composer_submission();
                                    if steering.text.trim() == "/exit" {
                                        break 'outer;
                                    }
                                    route_running_input(
                                        &mut state,
                                        if key.modifiers.contains(KeyModifiers::ALT) {
                                            None
                                        } else {
                                            current_steering.as_ref()
                                        },
                                        &mut queued_followup,
                                        steering,
                                    );
                                } else {
                                    if requires_model_before_send(&state, cwd) {
                                        state.transcript.push(TranscriptLine::new(Gutter::Note, "pick a model first"));
                                        open_picker(&mut state, config.model.provider.clone(), &picker_tx);
                                        continue 'outer;
                                    }
                                    let had_attachments = !state.pasted_attachments.is_empty()
                                        || !state.image_attachments.is_empty();
                                    let mut input = state.take_composer_submission();
                                    let custom = custom_commands::discover(cwd, BUILTIN_COMMAND_NAMES);
                                    let hints = if had_attachments {
                                        Vec::new()
                                    } else {
                                        slash_hint_items(&input.text, &custom)
                                    };
                                    if !hints.is_empty() {
                                        // Enter on a hint row completes to the highlighted command.
                                        input.text = hints[state.slash_selected.min(hints.len() - 1)].0.clone();
                                        state.slash_selected = 0;
                                    }
                                    let command = if had_attachments {
                                        None
                                    } else {
                                        parse_slash_command(&input.text)
                                    };
                                    if matches!(command.as_ref(), Some(SlashCommand::Exit)) {
                                        break 'outer;
                                    }
                                    if matches!(command.as_ref(), Some(SlashCommand::Map)) {
                                        spawn_repo_map(&state, &config, cwd, &events);
                                        continue 'outer;
                                    }
                                    if matches!(command.as_ref(), Some(SlashCommand::Index)) {
                                        spawn_background_index(&mut state, &config, cwd, &events);
                                        continue 'outer;
                                    }
                                    if matches!(command.as_ref(), Some(SlashCommand::Onboard)) {
                                        spawn_onboard(&state, &config, cwd, &events);
                                        continue 'outer;
                                    }
                                    if let Some(SlashCommand::Remember { team, text }) = command.as_ref() {
                                        let (team, text) = (*team, text.clone());
                                        if text.trim().is_empty() {
                                            state.transcript.push(TranscriptLine::new(
                                                Gutter::Note,
                                                "usage: /remember [--team] <text>",
                                            ));
                                        } else {
                                            spawn_save_command_memory(
                                                &state,
                                                &config,
                                                cwd,
                                                &memory_saved_tx,
                                                text,
                                                team,
                                            );
                                        }
                                        continue 'outer;
                                    }
                                    if let Some(cmd) = command {
                                        if let Some(expanded) =
                                            handle_slash_command(&mut state, cwd, &mut config, &picker_tx, cmd)
                                        {
                                            let submitted = submit_task(
                                                &mut state,
                                                cwd,
                                                &config,
                                                &cancel,
                                                &events,
                                                &handler,
                                                UserInput::text(expanded),
                                                true,
                                            );
                                            current_cancel = Some(submitted.cancel);
                                            current_steering = Some(submitted.steering);
                                        }
                                    } else if state.status.model.is_empty() {
                                        state.transcript.push(TranscriptLine::new(Gutter::Note, "pick a model first"));
                                        open_picker(&mut state, config.model.provider.clone(), &picker_tx);
                                    } else {
                                        let task = input;
                                        let submitted = submit_task(
                                            &mut state, cwd, &config, &cancel, &events, &handler, task,
                                            true,
                                        );
                                        current_cancel = Some(submitted.cancel);
                                        current_steering = Some(submitted.steering);
                                    }
                                }
                            }
                        }
                    }
                    Some(Ok(Event::Paste(pasted))) => {
                        if state.picker.open {
                            append_picker_filter(&mut state, &pasted);
                        } else {
                            append_paste_at(&mut state, cwd, &pasted);
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
                        let agent_error = match &ev {
                            KodeEvent::AgentError { message } => Some(message.clone()),
                            _ => None,
                        };
                        let had_partial_response = agent_error.is_some()
                            && (!state.response_buf.is_empty()
                                || !state.current_stream.is_empty()
                                || !state.stream_pending.is_empty());
                        let deferred_steering = match &ev {
                            KodeEvent::SteeringDeferred { messages } => Some(messages.clone()),
                            _ => None,
                        };
                        if matches!(ev, KodeEvent::IndexFinished { .. }) {
                            // Index prompt state on disk changed.
                            state.setup_dirty = true;
                        }
                        apply_event(&mut state, ev);
                        if let Some(messages) = deferred_steering {
                            for message in messages {
                                match queued_followup.as_mut() {
                                    Some(queued) => queued.append(message),
                                    None => queued_followup = Some(message),
                                }
                            }
                            state.transcript.push(TranscriptLine::new(
                                Gutter::Note,
                                "late steering moved to the next turn",
                            ));
                        }
                        if let Some(tool_calls) = finished_tool_calls {
                            record_completed_turn(
                                &mut state,
                                cwd,
                                &config.model.provider,
                                &config.model.model,
                                tool_calls,
                            );
                            spawn_memory_proposal(&state, &config, cwd, &memory_tx);
                            // Refresh the dirty flag + CURRENT CHANGE rows now
                            // that the task's edits (if any) have landed —
                            // same lazy poll as TUI start, no fixed interval.
                            spawn_git_poll(cwd.to_path_buf(), git_tx.clone());
                            current_cancel = None;
                            current_steering = None;
                            if let Some(task) = queued_followup.take() {
                                let submitted = submit_task(
                                    &mut state,
                                    cwd,
                                    &config,
                                    &cancel,
                                    &events,
                                    &handler,
                                    task,
                                    false,
                                );
                                current_cancel = Some(submitted.cancel);
                                current_steering = Some(submitted.steering);
                            }
                        } else if let Some(error) = agent_error {
                            // Preserve a truthful recovery surface: an agent
                            // may have edited files before it stopped.
                            spawn_git_poll(cwd.to_path_buf(), git_tx.clone());
                            record_failed_turn(
                                &mut state,
                                cwd,
                                &config.model.provider,
                                &config.model.model,
                                &error,
                                had_partial_response,
                            );
                            current_cancel = None;
                            current_steering = None;
                            if let Some(task) = queued_followup.take() {
                                let submitted = submit_task(
                                    &mut state,
                                    cwd,
                                    &config,
                                    &cancel,
                                    &events,
                                    &handler,
                                    task,
                                    false,
                                );
                                current_cancel = Some(submitted.cancel);
                                current_steering = Some(submitted.steering);
                            }
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
                    apply_picker_loaded(&mut state, loaded);
                }
            }

            repo = git_rx.recv() => {
                if let Some(repo) = repo {
                    apply_repo_state(&mut state, repo);
                }
            }

            // One-shot: the probe sends once and drops its sender. A closed
            // channel's `recv()` is always ready, so without this guard the
            // arm would fire on every poll and spin the loop.
            indexed = indexed_rx.recv(), if !indexed_probe_done => {
                indexed_probe_done = true;
                if let Some(answer) = indexed {
                    state.repo_indexed = answer;
                    state.setup_dirty = true;
                }
            }

            proposal = memory_rx.recv() => {
                if let Some(proposal) = proposal {
                    offer_memory(&mut state, proposal);
                }
            }

            saved = memory_saved_rx.recv() => {
                if let Some(result) = saved {
                    let text = match &result {
                        Ok(m) if m.team => "m ● saved · team".to_string(),
                        Ok(_) => "m ● saved".to_string(),
                        Err(e) => format!("memory not saved: {e}"),
                    };
                    state.transcript.push(TranscriptLine::new(Gutter::Ingat, text));
                    if let (Ok(SavedMemory { id, text: saved_text, team, turn: Some(index) }), Some(session)) =
                        (result, state.session_id.clone())
                    {
                        let entry = crate::ledger::LedgerEntry::Memory {
                            id,
                            text: saved_text,
                            team,
                        };
                        if let Some(turn) = state.history.get_mut(index) {
                            turn.ledger.push(entry.clone());
                        }
                        if let Err(e) = crate::session::amend_turn(cwd, &session, index, |t| {
                            t.ledger.push(entry)
                        }) {
                            state.transcript.push(TranscriptLine::new(
                                Gutter::Note,
                                format!("session amend failed (non-fatal): {e}"),
                            ));
                        }
                    }
                }
            }

            _ = ui_tick.tick() => {
                // Also the render-tick clock for the knowledge-band
                // dim→normal fade (item 3) — a new evidence row is dim for
                // its first 2 of these ~100ms ticks.
                state.render_tick = state.render_tick.wrapping_add(1);
                motion_tick_update(&mut state, Instant::now());
            }

            _ = motion_tick.tick(), if animating => {
                motion_tick_update(&mut state, Instant::now());
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

        maybe_show_setup_card(&mut state, &config, cwd);

        terminal.draw(|f| {
            draw(f, &mut state, cwd);
            theme::adapt(f.buffer_mut(), palette);
        })?;
    }

    if let Some(child) = current_cancel {
        child.cancel();
    }
    cancel.cancel();

    Ok(())
}

/// True while any Kode Benang animation can be in flight: a run is active, a
/// timed trace-back is pending, or one of the last 8 transcript lines is
/// inside its animation window. Never true under reduced motion.
fn motion_active(state: &AppState, now: Instant) -> bool {
    !state.reduced_motion
        && (state.running
            || matches!(state.trace_back, Some(Some(_)))
            || state
                .transcript
                .iter()
                .rev()
                .take(8)
                .any(|l| line_animating(l.born, now, false)))
}

/// Per-tick motion bookkeeping: expire a timed trace-back and sample the
/// token pulse.
fn motion_tick_update(state: &mut AppState, now: Instant) {
    if let Some(Some(until)) = state.trace_back
        && now >= until
    {
        state.trace_back = None;
        state.style_epoch += 1;
    }
    state.sample_pulse(now);
}

pub(crate) fn requires_model_before_send(state: &AppState, cwd: &Path) -> bool {
    if !state.status.model.is_empty() {
        return false;
    }
    let hint = if state.input.starts_with('/') {
        let custom = custom_commands::discover(cwd, BUILTIN_COMMAND_NAMES);
        let hints = slash_hint_items(&state.input, &custom);
        hints
            .get(state.slash_selected.min(hints.len().saturating_sub(1)))
            .map(|item| item.0.clone())
    } else {
        None
    };
    !state.pasted_attachments.is_empty()
        || !state.image_attachments.is_empty()
        || matches!(
            parse_slash_command(hint.as_deref().unwrap_or(&state.input)),
            None | Some(SlashCommand::Unknown(_) | SlashCommand::Custom { .. })
        )
}

/// Handles one key press. Returns `true` if the app should quit.
pub(crate) fn handle_key(
    state: &mut AppState,
    cwd: &Path,
    code: KeyCode,
    modifiers: KeyModifiers,
    current_cancel: &Option<CancellationToken>,
) -> bool {
    if code == KeyCode::Esc && state.why_lines.take().is_some() {
        return false;
    }
    if state.shortcuts_open {
        if code == KeyCode::Esc || code == KeyCode::Char('?') {
            state.shortcuts_open = false;
        }
        return false;
    }
    if state.attachments_open {
        if code == KeyCode::Esc
            || (modifiers.contains(KeyModifiers::CONTROL) && code == KeyCode::Char('a'))
        {
            state.attachments_open = false;
        }
        return false;
    }

    if !(modifiers.contains(KeyModifiers::CONTROL) && matches!(code, KeyCode::Char('c' | 'd'))) {
        state.exit_armed_at = None;
    }

    if !state.pending.is_empty() {
        let decision = match (code, modifiers) {
            (KeyCode::Char('a' | 'y'), KeyModifiers::NONE) => Some(true),
            (KeyCode::Char('d' | 'n') | KeyCode::Esc, KeyModifiers::NONE) => Some(false),
            _ => None,
        };
        if let Some(allow) = decision
            && let Some(req) = state.pop_permission()
        {
            let _ = req.responder.send(allow);
        }
        return false;
    }

    if code == KeyCode::Char('?') && state.input.is_empty() {
        state.shortcuts_open = true;
        return false;
    }

    if modifiers.contains(KeyModifiers::CONTROL)
        && code == KeyCode::Char('a')
        && state.pending.is_empty()
    {
        if !state.pasted_attachments.is_empty() || !state.image_attachments.is_empty() {
            state.attachments_open = true;
        }
        return false;
    }

    if modifiers.contains(KeyModifiers::CONTROL) && code == KeyCode::Char('c') {
        if !state.running {
            if !state.pending.is_empty() {
                return false;
            }
            if state.exit_confirmation_active() {
                return true;
            }
            state.exit_armed_at = Some(Instant::now());
            return false;
        }
        if let Some(c) = current_cancel {
            c.cancel();
        }
        return false;
    }

    if modifiers.contains(KeyModifiers::CONTROL) && code == KeyCode::Char('d') {
        if !state.input.is_empty() {
            state.delete_input();
        } else if !state.running {
            if state.exit_confirmation_active() {
                return true;
            }
            state.exit_armed_at = Some(Instant::now());
        }
        return false;
    }

    if modifiers.contains(KeyModifiers::CONTROL) && code == KeyCode::Char('p') {
        open_command_picker(state, cwd);
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
    let hint_count = if state.pending.is_empty()
        && !state.picker.open
        && state.pasted_attachments.is_empty()
        && state.image_attachments.is_empty()
        && state.input.starts_with('/')
    {
        let custom = custom_commands::discover(cwd, BUILTIN_COMMAND_NAMES);
        slash_hint_items(&state.input, &custom).len()
    } else {
        0
    };

    match code {
        KeyCode::Enter if modifiers.contains(KeyModifiers::SHIFT) => {
            state.insert_input("\n");
        }
        KeyCode::Esc => {
            if state.ledger_open {
                state.ledger_open = false;
            } else if state.running
                && let Some(c) = current_cancel
            {
                c.cancel();
            } else if hint_count > 0 {
                state.input.clear();
                state.input_cursor = None;
                state.slash_selected = 0;
            }
        }
        KeyCode::Tab if hint_count > 0 => {
            let custom = custom_commands::discover(cwd, BUILTIN_COMMAND_NAMES);
            let items = slash_hint_items(&state.input, &custom);
            let (name, _) = items[state.slash_selected.min(items.len() - 1)].clone();
            state.input = format!("{name} ");
            state.input_cursor = None;
            state.slash_selected = 0;
        }
        KeyCode::Char(c)
            if state.pending.is_empty()
                && !modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
        {
            state.insert_input(&c.to_string());
            state.slash_selected = 0;
        }
        KeyCode::Backspace => {
            if state.input.is_empty() {
                state.remove_last_attachment();
            } else {
                state.backspace_input();
            }
            state.slash_selected = 0;
        }
        KeyCode::Delete => {
            state.delete_input();
        }
        KeyCode::Left => state.move_input_left(),
        KeyCode::Right => state.move_input_right(),
        KeyCode::Home => state.move_input_home(),
        KeyCode::End => state.move_input_end(),
        KeyCode::Up => {
            if hint_count > 0 {
                state.slash_selected = state.slash_selected.saturating_sub(1);
            } else if !state.move_input_vertical(-1) && !state.history_previous() {
                state.scroll = state.scroll.saturating_sub(1);
                state.follow = false;
            }
        }
        KeyCode::Down => {
            if hint_count > 0 {
                state.slash_selected = (state.slash_selected + 1).min(hint_count - 1);
            } else if !state.move_input_vertical(1) && !state.history_next() {
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

#[cfg(test)]
mod native_log_tests {
    use super::*;

    #[tokio::test]
    #[ignore = "requires the installed zindeks library; run explicitly for terminal-log QA"]
    async fn embedded_watcher_log_goes_to_tui_log_file() {
        let root = std::env::temp_dir().join(format!(
            "kode-watcher-log-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("main.rs"), "fn main() {}\n").unwrap();
        let library =
            crate::engine_assets::zindeks_library(&KodeConfig::default().zindeks).unwrap();
        let log_path = kode_core::kode_home_dir()
            .unwrap()
            .join("logs/tui-stderr.log");
        let before = std::fs::metadata(&log_path).map(|m| m.len()).unwrap_or(0);
        let guard = TuiStderrGuard::redirect().unwrap();
        let engine =
            kode_intel::EmbeddedZindeks::open(&library, &root, &root.join("store"), true).unwrap();
        engine.index_repository().await.unwrap();
        drop(engine);
        drop(guard);
        let log = std::fs::read(&log_path).unwrap();
        assert!(
            String::from_utf8_lossy(&log[before as usize..]).contains("file watcher enabled"),
            "zindeks watcher log was not captured"
        );
    }
}

#[cfg(test)]
mod setup_card_tests {
    use super::*;

    fn idle_state() -> (AppState, KodeConfig, std::path::PathBuf) {
        let cwd = std::env::temp_dir().join(format!(
            "kode-setup-card-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&cwd).unwrap();
        let state = AppState::new(String::new(), String::new(), String::new());
        (state, KodeConfig::default(), cwd)
    }

    #[test]
    fn setup_card_waits_while_user_is_typing() {
        let (mut state, config, cwd) = idle_state();
        state.input = "half-typed task".to_string();
        maybe_show_setup_card(&mut state, &config, &cwd);
        assert!(!state.picker.open);
        assert!(state.setup_dirty, "typing must not consume the dirty flag");
    }

    #[test]
    fn setup_card_skips_disk_reads_when_clean() {
        let (mut state, config, cwd) = idle_state();
        state.setup_dirty = false;
        maybe_show_setup_card(&mut state, &config, &cwd);
        assert!(!state.picker.open);
    }

    #[test]
    fn setup_card_opens_once_when_dirty_and_idle() {
        let (mut state, config, cwd) = idle_state();
        maybe_show_setup_card(&mut state, &config, &cwd);
        assert!(state.picker.open);
        assert_eq!(state.picker.kind, PickerKind::Setup);
        assert!(!state.setup_dirty);
    }
}

#[cfg(test)]
mod failure_persistence_tests {
    use super::*;

    async fn panicking_task() -> anyhow::Result<()> {
        panic!("boom")
    }

    #[tokio::test]
    async fn guarded_task_converts_panic_to_recoverable_error() {
        assert_eq!(
            guard_task(panicking_task()).await.unwrap_err(),
            "agent task panicked; run recovered"
        );
    }

    #[test]
    fn failed_turn_keeps_partial_response_for_next_task() {
        let cwd = std::env::temp_dir().join(format!(
            "kode-failed-turn-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&cwd).unwrap();
        let mut state = AppState::new("codex".into(), "gpt-test".into(), String::new());
        state.start_new_task("continue phase 7", false);
        state.response_buf = "implemented prefault path".to_string();
        apply_event(
            &mut state,
            KodeEvent::AgentError {
                message: "model unavailable".into(),
            },
        );

        record_failed_turn(
            &mut state,
            &cwd,
            "codex",
            "gpt-test",
            "model unavailable",
            true,
        );

        assert_eq!(state.history.len(), 1);
        assert!(
            state.history[0]
                .response
                .contains("implemented prefault path")
        );
        assert!(
            state.history[0]
                .response
                .contains("run stopped: model unavailable")
        );
        let (turns, corrupt) =
            crate::session::load(&cwd, state.session_id.as_deref().expect("session created"))
                .unwrap();
        assert_eq!(corrupt, 0);
        assert_eq!(turns, state.history);
    }
}
