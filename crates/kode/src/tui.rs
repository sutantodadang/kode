mod theme;

use std::collections::VecDeque;
use std::io::Stdout;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use futures::StreamExt;
use kode_core::CancellationToken;
use kode_core::config::KodeConfig;
use kode_core::event::{EventBus, KodeEvent};
use kode_tools::permission::PermissionHandler;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use tokio::sync::{mpsc, oneshot};

use crate::pipeline;

/// The agent run's current phase, shown in the breadcrumb/spinner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunState {
    Idle,
    Thinking,
    Tool,
    Verify,
}

impl RunState {
    fn label(self) -> &'static str {
        match self {
            RunState::Idle => "idle",
            RunState::Thinking => "thinking",
            RunState::Tool => "tool",
            RunState::Verify => "verify",
        }
    }
}

#[derive(Debug, Clone)]
pub struct StatusInfo {
    pub provider: String,
    pub model: String,
    pub effort: String,
    pub context_tokens: usize,
    pub tools_used: u32,
    pub state: RunState,
}

impl StatusInfo {
    fn new(provider: String, model: String, effort: String) -> Self {
        Self {
            provider,
            model,
            effort,
            context_tokens: 0,
            tools_used: 0,
            state: RunState::Idle,
        }
    }
}

/// Provenance tag for one transcript line, rendered as a 2-col gutter
/// prefix (see `gutter_prefix`). Per `DESIGN.md`: color = provenance,
/// never decoration — never fake provenance on prose the sources didn't
/// produce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gutter {
    /// Blank spacer line — no glyph.
    None,
    /// Agent prose (flushed model token stream).
    Prose,
    /// A tool ran (started, or finished ok — no animation on success).
    Tool,
    /// A tool finished with an error.
    ToolFail,
    /// A verification result line. Not yet emitted by `apply_event` (the
    /// pipeline currently reports verification steps as generic `Note`
    /// events) — reserved so a future per-step Verify event can render
    /// distinctly without another gutter/glyph addition.
    #[allow(dead_code)]
    Verify,
    /// A progress/degradation note.
    Note,
    /// An agent-level error.
    Error,
    /// Echoed user input.
    User,
}

/// One line of transcript: its provenance gutter plus the rendered text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptLine {
    pub gutter: Gutter,
    pub text: String,
}

impl TranscriptLine {
    pub fn new(gutter: Gutter, text: impl Into<String>) -> Self {
        Self {
            gutter,
            text: text.into(),
        }
    }
}

/// The Knowledge Band's data — the last `KodeEvent::Knowledge` digest
/// received. Absent (`AppState::knowledge == None`) before the first
/// context compilation of the session.
#[derive(Debug, Clone, Default)]
pub struct KnowledgeState {
    pub zindeks: Vec<String>,
    pub ingat: Vec<String>,
    pub git: Vec<String>,
    pub context_tokens: usize,
    pub budget_tokens: usize,
}

/// A pending permission request awaiting a y/n answer from the user.
pub struct PermReq {
    pub summary: String,
    pub responder: oneshot::Sender<bool>,
}

/// State of the `/model` picker overlay. `items` holds the fetched catalog
/// (or is empty while loading / on fetch failure); `note` carries a status
/// line (loading / error) shown above the list.
#[derive(Debug, Clone, Default)]
pub struct PickerState {
    pub open: bool,
    pub filter: String,
    pub items: Vec<String>,
    pub selected: usize,
    pub note: Option<String>,
}

/// Result of a catalog fetch spawned when the picker opens, delivered back
/// over an internal channel (the fetch itself runs off the UI task).
#[derive(Debug, Clone)]
pub struct PickerLoaded {
    pub items: Vec<String>,
    pub error: Option<String>,
}

/// Pure UI state, driven by `apply_event`. Kept free of any terminal I/O so
/// it can be unit tested directly.
pub struct AppState {
    pub transcript: Vec<TranscriptLine>,
    pub current_stream: String,
    pub status: StatusInfo,
    pub running: bool,
    pub pending: VecDeque<PermReq>,
    pub scroll: u16,
    pub follow: bool,
    pub input: String,
    pub picker: PickerState,
    /// Last received Knowledge digest; `None` until the first context
    /// compilation of the session completes.
    pub knowledge: Option<KnowledgeState>,
    /// User-toggled visibility of the Knowledge Band (Ctrl+K).
    pub knowledge_band_open: bool,
    /// Last path component of the working directory, shown in the
    /// breadcrumb. Set once at startup.
    pub repo_dir: String,
    /// Current git branch (`git branch --show-current`), best-effort, read
    /// once at startup. `None` when not a git repo / git unavailable.
    pub branch: Option<String>,
    /// When the current run started, for the elapsed-time spinner label.
    pub run_started: Option<Instant>,
    /// Name of the tool currently running, if any (drives the spinner
    /// label: tool name vs. generic "thinking").
    pub current_tool: Option<String>,
}

impl AppState {
    pub fn new(provider: String, model: String, effort: String) -> Self {
        Self {
            transcript: Vec::new(),
            current_stream: String::new(),
            status: StatusInfo::new(provider, model, effort),
            running: false,
            pending: VecDeque::new(),
            scroll: 0,
            follow: true,
            input: String::new(),
            picker: PickerState::default(),
            knowledge: None,
            knowledge_band_open: true,
            repo_dir: String::new(),
            branch: None,
            run_started: None,
            current_tool: None,
        }
    }

    pub fn push_permission(&mut self, req: PermReq) {
        self.pending.push_back(req);
    }

    pub fn pop_permission(&mut self) -> Option<PermReq> {
        self.pending.pop_front()
    }
}

/// Applies one `KodeEvent` to `state`. Any accumulated `current_stream` text
/// is flushed into the transcript before non-token events are processed, so
/// the transcript always reads as a sequence of complete lines.
pub fn apply_event(state: &mut AppState, ev: KodeEvent) {
    if !matches!(ev, KodeEvent::ModelToken { .. }) && !state.current_stream.is_empty() {
        let text = std::mem::take(&mut state.current_stream);
        for line in text.split('\n') {
            if line.is_empty() {
                state.transcript.push(TranscriptLine::new(Gutter::None, ""));
            } else {
                state
                    .transcript
                    .push(TranscriptLine::new(Gutter::Prose, line));
            }
        }
    }

    match ev {
        KodeEvent::AgentStarted => {
            state.running = true;
            state.status.state = RunState::Thinking;
            state.run_started = Some(Instant::now());
            state.current_tool = None;
        }
        KodeEvent::ContextCompilationStarted => {}
        KodeEvent::ContextCompiled {
            token_estimate,
            sections: _,
        } => {
            state.status.context_tokens = token_estimate;
        }
        KodeEvent::ModelStarted => {
            state.status.state = RunState::Thinking;
            state.current_tool = None;
        }
        KodeEvent::ModelToken { text } => {
            state.current_stream.push_str(&text);
        }
        KodeEvent::ToolRequested { .. } => {}
        KodeEvent::ToolStarted { name } => {
            state
                .transcript
                .push(TranscriptLine::new(Gutter::Tool, name.clone()));
            state.status.tools_used += 1;
            state.status.state = RunState::Tool;
            state.current_tool = Some(name);
        }
        KodeEvent::ToolFinished { name, ok } => {
            if !ok {
                state.transcript.push(TranscriptLine::new(
                    Gutter::ToolFail,
                    format!("{name} failed"),
                ));
            }
            state.current_tool = None;
        }
        KodeEvent::VerificationStarted => {
            state.status.state = RunState::Verify;
            state.current_tool = None;
        }
        KodeEvent::VerificationFinished { .. } => {}
        KodeEvent::AgentFinished => {
            state.running = false;
            state.status.state = RunState::Idle;
            state.run_started = None;
            state.current_tool = None;
        }
        KodeEvent::AgentError { message } => {
            state
                .transcript
                .push(TranscriptLine::new(Gutter::Error, message));
            state.running = false;
            state.status.state = RunState::Idle;
            state.run_started = None;
            state.current_tool = None;
        }
        KodeEvent::Note { text } => {
            state
                .transcript
                .push(TranscriptLine::new(Gutter::Note, text));
        }
        KodeEvent::TaskFinished {
            iterations,
            tool_calls,
            input_tokens,
            output_tokens,
        } => {
            state.transcript.push(TranscriptLine::new(
                Gutter::Note,
                format!(
                    "{iterations} iterations, {tool_calls} tool calls, {input_tokens}→{output_tokens} tokens"
                ),
            ));
            state.running = false;
            state.status.state = RunState::Idle;
            state.run_started = None;
            state.current_tool = None;
        }
        KodeEvent::Knowledge {
            zindeks,
            ingat,
            git,
            context_tokens,
            budget_tokens,
        } => {
            state.knowledge = Some(KnowledgeState {
                zindeks,
                ingat,
                git,
                context_tokens,
                budget_tokens,
            });
        }
    }
}

/// A parsed `/`-prefixed slash command.
#[derive(Debug, Clone, PartialEq)]
pub enum SlashCommand {
    /// `/model` (open picker) or `/model <name>` (set directly).
    Model(Option<String>),
    /// `/effort <value>`.
    Effort(String),
    Help,
    Unknown(String),
}

/// Parses `input` as a slash command. Returns `None` when `input` doesn't
/// start with `/` — slash commands are only recognized at the start of the
/// line, per design.
pub fn parse_slash_command(input: &str) -> Option<SlashCommand> {
    let trimmed = input.trim();
    if !trimmed.starts_with('/') {
        return None;
    }
    let mut parts = trimmed.splitn(2, char::is_whitespace);
    let cmd = parts.next().unwrap_or("");
    let rest = parts.next().unwrap_or("").trim();
    Some(match cmd {
        "/model" => SlashCommand::Model(if rest.is_empty() {
            None
        } else {
            Some(rest.to_string())
        }),
        "/effort" => SlashCommand::Effort(rest.to_string()),
        "/help" => SlashCommand::Help,
        other => SlashCommand::Unknown(other.to_string()),
    })
}

/// Validates a reasoning-effort value against
/// [`kode_core::config::VALID_EFFORTS`]. Returns the value on success or an
/// error message listing the valid values.
pub fn validate_effort(value: &str) -> Result<String, String> {
    if kode_core::config::VALID_EFFORTS.contains(&value) {
        Ok(value.to_string())
    } else {
        Err(format!(
            "invalid effort '{value}' (valid: {})",
            kode_core::config::VALID_EFFORTS.join(", ")
        ))
    }
}

/// Filters `items` by `filter` using a case-insensitive substring match.
/// Empty `filter` yields all items, unchanged order.
pub fn picker_filtered_items(items: &[String], filter: &str) -> Vec<String> {
    if filter.is_empty() {
        return items.to_vec();
    }
    let needle = filter.to_lowercase();
    items
        .iter()
        .filter(|item| item.to_lowercase().contains(&needle))
        .cloned()
        .collect()
}

/// Decides what Enter selects in the picker: the highlighted row in
/// `filtered`, or — when there's no matching row and the filter text is
/// non-empty — the typed text verbatim. Returns `None` when there is
/// nothing to select (no rows, empty filter).
pub fn picker_enter_selection(
    filtered: &[String],
    filter: &str,
    selected: usize,
) -> Option<String> {
    if let Some(item) = filtered.get(selected) {
        return Some(item.clone());
    }
    let trimmed = filter.trim();
    if !trimmed.is_empty() {
        return Some(trimmed.to_string());
    }
    None
}

/// Effect requested by a key press while the picker is open.
#[derive(Debug, Clone, PartialEq)]
pub enum PickerOutcome {
    Continue,
    Select(String),
    Cancel,
}

/// Handles one key press while `state.picker.open`. Mutates the picker's
/// filter/selection in place; returns the effect (if any) the caller must
/// apply (selecting a model persists it and closes the picker).
pub fn handle_picker_key(state: &mut AppState, code: KeyCode) -> PickerOutcome {
    match code {
        KeyCode::Esc => PickerOutcome::Cancel,
        KeyCode::Enter => {
            let filtered = picker_filtered_items(&state.picker.items, &state.picker.filter);
            match picker_enter_selection(&filtered, &state.picker.filter, state.picker.selected) {
                Some(model) => PickerOutcome::Select(model),
                None => PickerOutcome::Continue,
            }
        }
        KeyCode::Up => {
            state.picker.selected = state.picker.selected.saturating_sub(1);
            PickerOutcome::Continue
        }
        KeyCode::Down => {
            let len = picker_filtered_items(&state.picker.items, &state.picker.filter).len();
            if len > 0 {
                state.picker.selected = (state.picker.selected + 1).min(len - 1);
            }
            PickerOutcome::Continue
        }
        KeyCode::Char(c) => {
            state.picker.filter.push(c);
            state.picker.selected = 0;
            PickerOutcome::Continue
        }
        KeyCode::Backspace => {
            state.picker.filter.pop();
            state.picker.selected = 0;
            PickerOutcome::Continue
        }
        _ => PickerOutcome::Continue,
    }
}

/// Opens the picker (clearing prior filter/items) and spawns a best-effort
/// catalog fetch for `provider`, delivered back via `tx`.
fn open_picker(state: &mut AppState, provider: String, tx: &mpsc::UnboundedSender<PickerLoaded>) {
    state.picker.open = true;
    state.picker.filter.clear();
    state.picker.selected = 0;
    state.picker.items.clear();
    state.picker.note = Some("loading models...".to_string());

    let tx = tx.clone();
    tokio::spawn(async move {
        let msg = match kode_model::catalog::list_models(&provider, None).await {
            Ok(items) => PickerLoaded { items, error: None },
            Err(e) => PickerLoaded {
                items: vec![],
                error: Some(e),
            },
        };
        let _ = tx.send(msg);
    });
}

/// Applies a parsed slash command to `state`/`config`. `/model` with no
/// argument opens the picker (async catalog fetch via `picker_tx`); every
/// other successful set persists immediately to
/// `<cwd>/.kode/config.toml` via [`KodeConfig::update_model_selection`].
fn handle_slash_command(
    state: &mut AppState,
    cwd: &Path,
    config: &mut KodeConfig,
    picker_tx: &mpsc::UnboundedSender<PickerLoaded>,
    cmd: SlashCommand,
) {
    match cmd {
        SlashCommand::Model(None) => {
            open_picker(state, config.model.provider.clone(), picker_tx);
        }
        SlashCommand::Model(Some(name)) => {
            state.status.model = name.clone();
            config.model.model = name.clone();
            let _ = KodeConfig::update_model_selection(cwd, Some(&name), None);
            state.transcript.push(TranscriptLine::new(
                Gutter::Note,
                format!("model set: {name}"),
            ));
        }
        SlashCommand::Effort(value) => match validate_effort(&value) {
            Ok(v) => {
                state.status.effort = v.clone();
                config.model.effort = v.clone();
                let _ = KodeConfig::update_model_selection(cwd, None, Some(&v));
                state.transcript.push(TranscriptLine::new(
                    Gutter::Note,
                    format!("effort set: {v}"),
                ));
            }
            Err(msg) => state
                .transcript
                .push(TranscriptLine::new(Gutter::Note, msg)),
        },
        SlashCommand::Help => {
            state.transcript.push(TranscriptLine::new(
                Gutter::Note,
                "commands: /model [name], /effort <minimal|low|medium|high|xhigh>, /help",
            ));
        }
        SlashCommand::Unknown(cmd) => {
            state.transcript.push(TranscriptLine::new(
                Gutter::Note,
                format!("unknown command: {cmd}"),
            ));
        }
    }
}

/// Sends permission requests from tool execution to the UI loop, then awaits
/// the user's y/n answer over a one-shot channel.
pub struct TuiPermission {
    tx: mpsc::UnboundedSender<(String, oneshot::Sender<bool>)>,
}

impl TuiPermission {
    pub fn new(tx: mpsc::UnboundedSender<(String, oneshot::Sender<bool>)>) -> Self {
        Self { tx }
    }
}

#[async_trait::async_trait]
impl PermissionHandler for TuiPermission {
    async fn confirm(&self, summary: &str) -> bool {
        let (resp_tx, resp_rx) = oneshot::channel();
        if self.tx.send((summary.to_string(), resp_tx)).is_err() {
            return false;
        }
        resp_rx.await.unwrap_or(false)
    }
}

/// Restores the terminal to its normal mode on drop, including on panic
/// unwind.
struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(std::io::stdout(), LeaveAlternateScreen);
    }
}

/// Best-effort current branch via `git branch --show-current`. `None` when
/// not a git repo, git is unavailable, or the repo has no commits yet.
fn detect_branch(cwd: &Path) -> Option<String> {
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

/// Launches the interactive TUI. Runs until the user quits (Ctrl-C/'q' while
/// idle) or the process is otherwise terminated.
pub async fn run(cwd: &Path, cancel: CancellationToken) -> anyhow::Result<()> {
    let mut config = KodeConfig::load(cwd).unwrap_or_default();

    enable_raw_mode()?;
    execute!(std::io::stdout(), EnterAlternateScreen)?;
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

    let (perm_tx, mut perm_rx) = mpsc::unbounded_channel::<(String, oneshot::Sender<bool>)>();
    let handler: Arc<dyn PermissionHandler> = Arc::new(TuiPermission::new(perm_tx));

    let (picker_tx, mut picker_rx) = mpsc::unbounded_channel::<PickerLoaded>();

    let events = EventBus::new(256);
    let mut event_rx = events.subscribe();

    let mut key_events = EventStream::new();
    let mut current_cancel: Option<CancellationToken> = None;

    terminal.draw(|f| draw(f, &state))?;

    'outer: loop {
        tokio::select! {
            biased;

            maybe_key = key_events.next() => {
                match maybe_key {
                    Some(Ok(Event::Key(key))) if key.kind == KeyEventKind::Press => {
                        if state.picker.open {
                            match handle_picker_key(&mut state, key.code) {
                                PickerOutcome::Select(model) => {
                                    state.status.model = model.clone();
                                    config.model.model = model.clone();
                                    let _ = KodeConfig::update_model_selection(cwd, Some(&model), None);
                                    state.transcript.push(TranscriptLine::new(
                                        Gutter::Note,
                                        format!("model set: {model}"),
                                    ));
                                    state.picker.open = false;
                                }
                                PickerOutcome::Cancel => {
                                    state.picker.open = false;
                                }
                                PickerOutcome::Continue => {}
                            }
                        } else {
                            if handle_key(&mut state, key.code, key.modifiers, &current_cancel) {
                                break 'outer;
                            }
                            if key.code == KeyCode::Enter && !state.running && !state.input.trim().is_empty() {
                                let input = std::mem::take(&mut state.input);
                                if let Some(cmd) = parse_slash_command(&input) {
                                    handle_slash_command(&mut state, cwd, &mut config, &picker_tx, cmd);
                                } else if state.status.model.is_empty() {
                                    state.transcript.push(TranscriptLine::new(Gutter::Note, "pick a model first"));
                                    open_picker(&mut state, config.model.provider.clone(), &picker_tx);
                                } else {
                                    let task = input;
                                    state
                                        .transcript
                                        .push(TranscriptLine::new(Gutter::User, task.clone()));
                                    let child = cancel.child_token();
                                    current_cancel = Some(child.clone());
                                    state.running = true;
                                    state.status.state = RunState::Thinking;

                                    let task_events = events.clone();
                                    let task_cwd = cwd.to_path_buf();
                                    let task_config = config.clone();
                                    let task_handler = handler.clone();
                                    tokio::spawn(async move {
                                        if let Err(err) = pipeline::run_task(
                                            &task,
                                            &task_cwd,
                                            &task_config,
                                            task_events.clone(),
                                            task_handler,
                                            child,
                                        )
                                        .await
                                        {
                                            task_events.emit(KodeEvent::AgentError {
                                                message: err.to_string(),
                                            });
                                        }
                                    });
                                }
                            }
                        }
                    }
                    Some(Ok(_)) => {}
                    Some(Err(_)) | None => break 'outer,
                }
            }

            ev = event_rx.recv() => {
                if let Ok(ev) = ev {
                    apply_event(&mut state, ev);
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
        }

        terminal.draw(|f| draw(f, &state))?;
    }

    if let Some(child) = current_cancel {
        child.cancel();
    }
    cancel.cancel();

    Ok(())
}

/// Handles one key press. Returns `true` if the app should quit.
fn handle_key(
    state: &mut AppState,
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

    match code {
        KeyCode::Esc => {
            if state.running
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
        KeyCode::Char(c) => {
            state.input.push(c);
        }
        KeyCode::Backspace => {
            state.input.pop();
        }
        KeyCode::Up => {
            state.scroll = state.scroll.saturating_sub(1);
            state.follow = false;
        }
        KeyCode::Down => {
            state.scroll = state.scroll.saturating_add(1);
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

/// Renders an 8-cell (by default) meter string: `▓` for filled cells,
/// `░` for empty ones. `budget == 0` yields an all-empty meter (no
/// division by zero). Pure — the caller applies Z/DIM coloring per cell.
pub fn meter(used: usize, budget: usize, cells: usize) -> String {
    let filled = if budget == 0 {
        0
    } else {
        let ratio = used as f64 / budget as f64;
        ((ratio * cells as f64).round() as usize).min(cells)
    };
    let mut s = String::with_capacity(cells);
    for i in 0..cells {
        s.push(if i < filled { '▓' } else { '░' });
    }
    s
}

/// True when the Knowledge Band should render: the user hasn't collapsed
/// it (Ctrl+K), a `Knowledge` event has arrived, and at least one source
/// has data. Per `DESIGN.md`: never fake provenance — the band is hidden
/// entirely when a source is unavailable/empty, never shown padded/empty.
pub fn knowledge_band_visible(state: &AppState) -> bool {
    state.knowledge_band_open
        && state
            .knowledge
            .as_ref()
            .is_some_and(|k| !k.zindeks.is_empty() || !k.ingat.is_empty() || !k.git.is_empty())
}

/// Formats a token count compactly: `<1000` verbatim, else `"{n:.1}k"`.
fn format_k(n: usize) -> String {
    if n < 1000 {
        n.to_string()
    } else {
        format!("{:.1}k", n as f64 / 1000.0)
    }
}

const SPINNER_FRAMES: [char; 4] = ['·', '•', '●', '•'];

/// The single spinner instance's frame for `elapsed_ms`, cycling at 4 Hz
/// (one of the 4 frames every 250ms). Per `DESIGN.md`: ONE moving region
/// max, exactly these frames.
fn spinner_frame(elapsed_ms: u128) -> char {
    let idx = ((elapsed_ms / 250) % 4) as usize;
    SPINNER_FRAMES[idx]
}

/// Maps a `Gutter` to its fixed 2-col glyph prefix and color, per
/// `DESIGN.md`'s glyph vocabulary. Every color pairs with a fixed glyph —
/// shape carries meaning without color.
fn gutter_prefix(gutter: &Gutter) -> (&'static str, Color) {
    match gutter {
        Gutter::None => ("  ", Color::Reset),
        Gutter::Prose => ("│ ", theme::DIM),
        Gutter::Tool => ("T▸", theme::T),
        Gutter::ToolFail => ("T▸", theme::ERR),
        Gutter::Verify => ("V ", theme::OK),
        Gutter::Note => ("· ", theme::DIM),
        Gutter::Error => ("× ", theme::ERR),
        Gutter::User => ("› ", theme::MUTED),
    }
}

/// Renders one transcript line as a gutter span + text span. Long lines
/// wrap via `Paragraph`'s own word-wrap without gutter-aligned
/// continuation (acceptable ceiling for this phase).
fn transcript_line_to_ratatui(line: &TranscriptLine) -> Line<'static> {
    let (prefix, color) = gutter_prefix(&line.gutter);
    Line::from(vec![
        Span::styled(prefix, Style::default().fg(color)),
        Span::raw(line.text.clone()),
    ])
}

/// Builds the breadcrumb row: `kode  {repo} · {branch} · {provider}/{model}
/// · effort:{e} · ctx ▓▓░░ {used}/{budget}`. The context meter is omitted
/// until the first `Knowledge` event of the session.
fn breadcrumb_line(state: &AppState) -> Line<'static> {
    let branch = state.branch.clone().unwrap_or_else(|| "no git".to_string());
    let effort = if state.status.effort.is_empty() {
        "-".to_string()
    } else {
        state.status.effort.clone()
    };
    let model = if state.status.model.is_empty() {
        "(no model)".to_string()
    } else {
        state.status.model.clone()
    };
    let prefix = format!(
        " kode  {} · {branch} · {}/{model} · effort:{effort}",
        state.repo_dir, state.status.provider
    );
    let mut spans = vec![Span::raw(prefix)];
    if let Some(ks) = &state.knowledge {
        let bar = meter(ks.context_tokens, ks.budget_tokens, 8);
        let filled = bar.chars().filter(|c| *c == '▓').count();
        spans.push(Span::raw(" · ctx "));
        spans.push(Span::styled(
            bar.chars().take(filled).collect::<String>(),
            Style::default().fg(theme::Z),
        ));
        spans.push(Span::styled(
            bar.chars().skip(filled).collect::<String>(),
            Style::default().fg(theme::DIM),
        ));
        spans.push(Span::raw(format!(
            " {}/{}",
            format_k(ks.context_tokens),
            format_k(ks.budget_tokens)
        )));
    }
    Line::from(spans)
}

/// Builds the Knowledge Band's content lines (not including the trailing
/// rule line, which needs the render-time area width). Row 1: `Z {first
/// zindeks fact}`, indented continuation lines for the rest; then one `I`
/// line per ingat summary (quoted, italic); then one `G` line for the git
/// impact. Sources with empty vecs render nothing.
fn knowledge_band_lines(ks: &KnowledgeState) -> Vec<Line<'static>> {
    let mut lines: Vec<Line> = Vec::new();

    if let Some((first, rest)) = ks.zindeks.split_first() {
        lines.push(Line::from(vec![
            Span::raw(" KNOWS  "),
            Span::styled(
                format!("Z {first}"),
                Style::default().fg(theme::Z).add_modifier(Modifier::BOLD),
            ),
        ]));
        for extra in rest {
            lines.push(Line::from(Span::styled(
                format!("          {extra}"),
                Style::default().fg(theme::Z),
            )));
        }
    }

    for entry in &ks.ingat {
        lines.push(Line::from(vec![
            Span::raw(" KNOWS  "),
            Span::styled("I ", Style::default().fg(theme::I)),
            Span::styled(
                format!("\u{201c}{entry}\u{201d}"),
                Style::default().fg(theme::I).add_modifier(Modifier::ITALIC),
            ),
        ]));
    }

    if let Some(git_line) = ks.git.first() {
        lines.push(Line::from(vec![
            Span::raw(" KNOWS  "),
            Span::styled("G ", Style::default().fg(theme::G)),
            Span::styled(git_line.clone(), Style::default().fg(theme::G)),
        ]));
    }

    lines
}

/// Renders the Knowledge Band into `area`: `lines`' content plus a
/// trailing DIM `─` rule spanning `area`'s full width — the only
/// horizontal rule besides the one above the input box.
fn draw_knowledge_band(
    f: &mut ratatui::Frame,
    area: ratatui::layout::Rect,
    mut lines: Vec<Line<'static>>,
) {
    let rule: String = "─".repeat(area.width as usize);
    lines.push(Line::from(Span::styled(
        rule,
        Style::default().fg(theme::DIM),
    )));
    f.render_widget(Paragraph::new(lines), area);
}

fn draw(f: &mut ratatui::Frame, state: &AppState) {
    let band_lines = if knowledge_band_visible(state) {
        state.knowledge.as_ref().map(knowledge_band_lines)
    } else {
        None
    };
    let band_height = band_lines.as_ref().map(|l| l.len() as u16 + 1).unwrap_or(0);

    let has_pending = !state.pending.is_empty();

    let mut constraints = vec![Constraint::Length(1)]; // breadcrumb
    if band_height > 0 {
        constraints.push(Constraint::Length(band_height));
    }
    constraints.push(Constraint::Min(1)); // transcript
    if has_pending {
        constraints.push(Constraint::Length(3));
    }
    constraints.push(Constraint::Length(3)); // input

    let areas = Layout::vertical(constraints).split(f.area());
    let mut idx = 0;

    f.render_widget(Paragraph::new(breadcrumb_line(state)), areas[idx]);
    idx += 1;

    if let Some(lines) = band_lines {
        draw_knowledge_band(f, areas[idx], lines);
        idx += 1;
    }

    let mut text_lines: Vec<Line> = state
        .transcript
        .iter()
        .map(transcript_line_to_ratatui)
        .collect();
    if !state.current_stream.is_empty() {
        text_lines.push(transcript_line_to_ratatui(&TranscriptLine::new(
            Gutter::Prose,
            state.current_stream.clone(),
        )));
    }
    if state.running {
        let elapsed = state.run_started.map(|t| t.elapsed()).unwrap_or_default();
        let frame = spinner_frame(elapsed.as_millis());
        let secs = elapsed.as_secs_f64();
        let label = match &state.current_tool {
            Some(tool) => format!("▸ {tool} · {secs:.1}s"),
            None => format!("{frame} {} · {secs:.1}s", state.status.state.label()),
        };
        text_lines.push(Line::from(Span::styled(
            label,
            Style::default().fg(theme::T),
        )));
    }
    let transcript = Paragraph::new(text_lines)
        .wrap(Wrap { trim: false })
        .scroll((state.scroll, 0))
        .block(Block::default().borders(Borders::NONE));
    f.render_widget(transcript, areas[idx]);
    idx += 1;

    let next_area = if has_pending {
        let req_line = state
            .pending
            .front()
            .map(|r| format!("allow: {}  [y]es / [n]o", r.summary))
            .unwrap_or_default();
        f.render_widget(
            Paragraph::new(Span::styled(
                req_line,
                Style::default().fg(theme::I).add_modifier(Modifier::BOLD),
            )),
            areas[idx],
        );
        idx += 1;
        areas[idx]
    } else {
        areas[idx]
    };

    let input = Paragraph::new(state.input.as_str())
        .block(Block::default().borders(Borders::ALL).title("task"));
    f.render_widget(input, next_area);

    if state.picker.open {
        draw_picker(f, &state.picker);
    }
}

/// Centered overlay listing (filtered) catalog entries, max ~12 rows
/// visible. `>` marks the currently-selected row.
fn draw_picker(f: &mut ratatui::Frame, picker: &PickerState) {
    let area = f.area();
    let width = area.width.saturating_mul(3) / 4;
    let width = width.clamp(20.min(area.width), area.width);
    let height = 16u16
        .min(area.height.saturating_sub(2))
        .max(5.min(area.height));
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let popup = ratatui::layout::Rect {
        x,
        y,
        width,
        height,
    };

    f.render_widget(Clear, popup);

    let filtered = picker_filtered_items(&picker.items, &picker.filter);
    let mut lines: Vec<Line> = vec![
        Line::from(format!("filter: {}", picker.filter)),
        Line::from("(type to filter; Enter on empty filter row = use typed text verbatim)"),
    ];
    if let Some(note) = &picker.note {
        lines.push(Line::from(format!("note: {note}")));
    }
    if filtered.is_empty() && picker.items.is_empty() && picker.note.is_none() {
        lines.push(Line::from("(loading...)"));
    }
    for (i, item) in filtered.iter().take(12).enumerate() {
        let marker = if i == picker.selected { "> " } else { "  " };
        lines.push(Line::from(format!("{marker}{item}")));
    }

    let block = Block::default().borders(Borders::ALL).title("select model");
    let paragraph = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false });
    f.render_widget(paragraph, popup);
}

#[allow(dead_code)]
type Backend = CrosstermBackend<Stdout>;

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> AppState {
        AppState::new("openai".to_string(), "gpt-test".to_string(), String::new())
    }

    #[test]
    fn model_token_accumulates_into_current_stream() {
        let mut s = state();
        apply_event(&mut s, KodeEvent::ModelToken { text: "hel".into() });
        apply_event(&mut s, KodeEvent::ModelToken { text: "lo".into() });
        assert_eq!(s.current_stream, "hello");
        assert!(s.transcript.is_empty());
    }

    #[test]
    fn tool_started_flushes_current_stream_and_pushes_line() {
        let mut s = state();
        apply_event(
            &mut s,
            KodeEvent::ModelToken {
                text: "thinking...".into(),
            },
        );
        apply_event(
            &mut s,
            KodeEvent::ToolStarted {
                name: "read_file".into(),
            },
        );
        assert_eq!(
            s.transcript,
            vec![
                TranscriptLine::new(Gutter::Prose, "thinking..."),
                TranscriptLine::new(Gutter::Tool, "read_file"),
            ]
        );
        assert!(s.current_stream.is_empty());
        assert_eq!(s.status.tools_used, 1);
        assert_eq!(s.status.state, RunState::Tool);
        assert_eq!(s.current_tool, Some("read_file".to_string()));
    }

    #[test]
    fn tool_finished_failure_pushes_failure_line() {
        let mut s = state();
        apply_event(
            &mut s,
            KodeEvent::ToolFinished {
                name: "run_shell".into(),
                ok: false,
            },
        );
        assert_eq!(
            s.transcript,
            vec![TranscriptLine::new(Gutter::ToolFail, "run_shell failed")]
        );
        assert_eq!(s.current_tool, None);
    }

    #[test]
    fn tool_finished_success_pushes_nothing() {
        let mut s = state();
        apply_event(
            &mut s,
            KodeEvent::ToolFinished {
                name: "run_shell".into(),
                ok: true,
            },
        );
        assert!(s.transcript.is_empty());
    }

    #[test]
    fn note_pushes_note_gutter_line() {
        let mut s = state();
        apply_event(
            &mut s,
            KodeEvent::Note {
                text: "code intelligence unavailable".into(),
            },
        );
        assert_eq!(
            s.transcript,
            vec![TranscriptLine::new(
                Gutter::Note,
                "code intelligence unavailable"
            )]
        );
    }

    #[test]
    fn context_compiled_updates_token_counter() {
        let mut s = state();
        apply_event(
            &mut s,
            KodeEvent::ContextCompiled {
                token_estimate: 4321,
                sections: 3,
            },
        );
        assert_eq!(s.status.context_tokens, 4321);
    }

    #[test]
    fn task_finished_updates_counters_and_ends_run() {
        let mut s = state();
        s.running = true;
        s.status.state = RunState::Verify;
        apply_event(
            &mut s,
            KodeEvent::TaskFinished {
                iterations: 3,
                tool_calls: 5,
                input_tokens: 100,
                output_tokens: 50,
            },
        );
        assert!(!s.running);
        assert_eq!(s.status.state, RunState::Idle);
        assert_eq!(
            s.transcript,
            vec![TranscriptLine::new(
                Gutter::Note,
                "3 iterations, 5 tool calls, 100→50 tokens"
            )]
        );
    }

    #[test]
    fn agent_started_sets_running_and_thinking() {
        let mut s = state();
        apply_event(&mut s, KodeEvent::AgentStarted);
        assert!(s.running);
        assert_eq!(s.status.state, RunState::Thinking);
        assert!(s.run_started.is_some());
    }

    #[test]
    fn agent_finished_clears_running() {
        let mut s = state();
        s.running = true;
        apply_event(&mut s, KodeEvent::AgentFinished);
        assert!(!s.running);
        assert_eq!(s.status.state, RunState::Idle);
        assert!(s.run_started.is_none());
    }

    #[test]
    fn agent_error_pushes_error_line_and_clears_running() {
        let mut s = state();
        s.running = true;
        apply_event(
            &mut s,
            KodeEvent::AgentError {
                message: "boom".into(),
            },
        );
        assert!(!s.running);
        assert_eq!(
            s.transcript,
            vec![TranscriptLine::new(Gutter::Error, "boom")]
        );
    }

    #[test]
    fn knowledge_event_updates_band_state_without_transcript_push() {
        let mut s = state();
        apply_event(
            &mut s,
            KodeEvent::Knowledge {
                zindeks: vec!["src/foo.rs (0.9)".to_string()],
                ingat: vec!["always prefix with rtk".to_string()],
                git: vec!["3 files changed".to_string()],
                context_tokens: 4200,
                budget_tokens: 16_000,
            },
        );
        assert!(s.transcript.is_empty());
        let ks = s.knowledge.as_ref().expect("knowledge set");
        assert_eq!(ks.zindeks, vec!["src/foo.rs (0.9)".to_string()]);
        assert_eq!(ks.ingat, vec!["always prefix with rtk".to_string()]);
        assert_eq!(ks.git, vec!["3 files changed".to_string()]);
        assert_eq!(ks.context_tokens, 4200);
        assert_eq!(ks.budget_tokens, 16_000);
    }

    #[test]
    fn permission_queue_push_pop_is_fifo() {
        let mut s = state();
        let (tx1, _rx1) = oneshot::channel();
        let (tx2, _rx2) = oneshot::channel();
        s.push_permission(PermReq {
            summary: "run rm -rf".into(),
            responder: tx1,
        });
        s.push_permission(PermReq {
            summary: "write file".into(),
            responder: tx2,
        });
        assert_eq!(s.pending.len(), 2);
        let first = s.pop_permission().unwrap();
        assert_eq!(first.summary, "run rm -rf");
        let second = s.pop_permission().unwrap();
        assert_eq!(second.summary, "write file");
        assert!(s.pop_permission().is_none());
    }

    #[tokio::test]
    async fn handle_key_y_resolves_pending_permission() {
        let mut s = state();
        let (tx, rx) = oneshot::channel();
        s.push_permission(PermReq {
            summary: "allow?".into(),
            responder: tx,
        });
        let quit = handle_key(&mut s, KeyCode::Char('y'), KeyModifiers::NONE, &None);
        assert!(!quit);
        assert!(s.pending.is_empty());
        assert!(rx.await.unwrap());
    }

    #[tokio::test]
    async fn handle_key_n_resolves_pending_permission_as_false() {
        let mut s = state();
        let (tx, rx) = oneshot::channel();
        s.push_permission(PermReq {
            summary: "allow?".into(),
            responder: tx,
        });
        let quit = handle_key(&mut s, KeyCode::Char('n'), KeyModifiers::NONE, &None);
        assert!(!quit);
        assert!(!rx.await.unwrap());
    }

    #[test]
    fn handle_key_q_quits_when_idle_and_input_empty() {
        let mut s = state();
        assert!(handle_key(
            &mut s,
            KeyCode::Char('q'),
            KeyModifiers::NONE,
            &None
        ));
    }

    #[test]
    fn handle_key_q_types_when_input_nonempty() {
        let mut s = state();
        s.input.push_str("say ");
        assert!(!handle_key(
            &mut s,
            KeyCode::Char('q'),
            KeyModifiers::NONE,
            &None
        ));
        assert_eq!(s.input, "say q");
    }

    #[test]
    fn handle_key_backspace_edits_input() {
        let mut s = state();
        s.input.push_str("abc");
        handle_key(&mut s, KeyCode::Backspace, KeyModifiers::NONE, &None);
        assert_eq!(s.input, "ab");
    }

    #[test]
    fn handle_key_scroll_updates_position_and_follow() {
        let mut s = state();
        s.scroll = 5;
        handle_key(&mut s, KeyCode::Up, KeyModifiers::NONE, &None);
        assert_eq!(s.scroll, 4);
        assert!(!s.follow);
        handle_key(&mut s, KeyCode::PageDown, KeyModifiers::NONE, &None);
        assert_eq!(s.scroll, 14);
    }

    #[test]
    fn handle_key_ctrl_k_toggles_knowledge_band() {
        let mut s = state();
        assert!(s.knowledge_band_open);
        handle_key(&mut s, KeyCode::Char('k'), KeyModifiers::CONTROL, &None);
        assert!(!s.knowledge_band_open);
        handle_key(&mut s, KeyCode::Char('k'), KeyModifiers::CONTROL, &None);
        assert!(s.knowledge_band_open);
    }

    // -- knowledge band visibility ----------------------------------------

    #[test]
    fn knowledge_band_visible_false_when_no_knowledge_yet() {
        let s = state();
        assert!(!knowledge_band_visible(&s));
    }

    #[test]
    fn knowledge_band_visible_false_when_all_sources_empty() {
        let mut s = state();
        s.knowledge = Some(KnowledgeState::default());
        assert!(!knowledge_band_visible(&s));
    }

    #[test]
    fn knowledge_band_visible_true_when_a_source_has_data() {
        let mut s = state();
        s.knowledge = Some(KnowledgeState {
            zindeks: vec!["src/foo.rs".to_string()],
            ..Default::default()
        });
        assert!(knowledge_band_visible(&s));
    }

    #[test]
    fn knowledge_band_visible_false_when_toggled_closed() {
        let mut s = state();
        s.knowledge = Some(KnowledgeState {
            zindeks: vec!["src/foo.rs".to_string()],
            ..Default::default()
        });
        s.knowledge_band_open = false;
        assert!(!knowledge_band_visible(&s));
    }

    // -- breadcrumb meter ---------------------------------------------------

    #[test]
    fn meter_zero_percent() {
        assert_eq!(meter(0, 16_000, 8), "░░░░░░░░");
    }

    #[test]
    fn meter_fifty_percent() {
        assert_eq!(meter(8_000, 16_000, 8), "▓▓▓▓░░░░");
    }

    #[test]
    fn meter_hundred_percent() {
        assert_eq!(meter(16_000, 16_000, 8), "▓▓▓▓▓▓▓▓");
    }

    #[test]
    fn meter_zero_budget_is_all_empty() {
        assert_eq!(meter(500, 0, 8), "░░░░░░░░");
    }

    // -- gutter mapping -------------------------------------------------

    #[test]
    fn gutter_prefix_matches_glyph_vocabulary() {
        assert_eq!(gutter_prefix(&Gutter::None).0, "  ");
        assert_eq!(gutter_prefix(&Gutter::Prose).0, "│ ");
        assert_eq!(gutter_prefix(&Gutter::Tool).0, "T▸");
        assert_eq!(gutter_prefix(&Gutter::ToolFail).0, "T▸");
        assert_eq!(gutter_prefix(&Gutter::Verify).0, "V ");
        assert_eq!(gutter_prefix(&Gutter::Note).0, "· ");
        assert_eq!(gutter_prefix(&Gutter::Error).0, "× ");
        assert_eq!(gutter_prefix(&Gutter::User).0, "› ");
    }

    // -- slash commands -----------------------------------------------

    #[test]
    fn parse_slash_command_non_slash_input_is_none() {
        assert_eq!(parse_slash_command("do the thing"), None);
    }

    #[test]
    fn parse_slash_command_model_with_no_arg() {
        assert_eq!(
            parse_slash_command("/model"),
            Some(SlashCommand::Model(None))
        );
    }

    #[test]
    fn parse_slash_command_model_with_arg() {
        assert_eq!(
            parse_slash_command("/model gpt-5.6-sol"),
            Some(SlashCommand::Model(Some("gpt-5.6-sol".to_string())))
        );
    }

    #[test]
    fn parse_slash_command_effort() {
        assert_eq!(
            parse_slash_command("/effort high"),
            Some(SlashCommand::Effort("high".to_string()))
        );
    }

    #[test]
    fn parse_slash_command_help() {
        assert_eq!(parse_slash_command("/help"), Some(SlashCommand::Help));
    }

    #[test]
    fn parse_slash_command_unknown() {
        assert_eq!(
            parse_slash_command("/nonsense arg"),
            Some(SlashCommand::Unknown("/nonsense".to_string()))
        );
    }

    #[test]
    fn validate_effort_accepts_known_values() {
        for v in kode_core::config::VALID_EFFORTS {
            assert_eq!(validate_effort(v), Ok(v.to_string()));
        }
    }

    #[test]
    fn validate_effort_rejects_unknown_value() {
        let err = validate_effort("banana").unwrap_err();
        assert!(err.contains("banana"));
        assert!(err.contains("minimal"));
    }

    // -- picker ----------------------------------------------------------

    #[test]
    fn picker_filtered_items_empty_filter_returns_all() {
        let items = vec!["a".to_string(), "b".to_string()];
        assert_eq!(picker_filtered_items(&items, ""), items);
    }

    #[test]
    fn picker_filtered_items_narrows_by_substring_case_insensitive() {
        let items = vec![
            "gpt-5.6-sol".to_string(),
            "gpt-5.6-codex".to_string(),
            "kimi-k3".to_string(),
        ];
        let filtered = picker_filtered_items(&items, "CODEX");
        assert_eq!(filtered, vec!["gpt-5.6-codex".to_string()]);
    }

    #[test]
    fn picker_enter_selection_picks_highlighted_row_when_present() {
        let filtered = vec!["gpt-5.6-sol".to_string(), "gpt-5.6-codex".to_string()];
        let selected = picker_enter_selection(&filtered, "gpt", 1);
        assert_eq!(selected, Some("gpt-5.6-codex".to_string()));
    }

    #[test]
    fn picker_enter_selection_uses_typed_text_when_no_match() {
        let filtered: Vec<String> = vec![];
        let selected = picker_enter_selection(&filtered, "custom-model", 0);
        assert_eq!(selected, Some("custom-model".to_string()));
    }

    #[test]
    fn picker_enter_selection_none_when_no_match_and_empty_filter() {
        let filtered: Vec<String> = vec![];
        let selected = picker_enter_selection(&filtered, "", 0);
        assert_eq!(selected, None);
    }

    #[test]
    fn handle_picker_key_typing_updates_filter_and_resets_selection() {
        let mut s = state();
        s.picker.selected = 3;
        handle_picker_key(&mut s, KeyCode::Char('x'));
        assert_eq!(s.picker.filter, "x");
        assert_eq!(s.picker.selected, 0);
    }

    #[test]
    fn handle_picker_key_backspace_edits_filter() {
        let mut s = state();
        s.picker.filter = "abc".to_string();
        handle_picker_key(&mut s, KeyCode::Backspace);
        assert_eq!(s.picker.filter, "ab");
    }

    #[test]
    fn handle_picker_key_down_clamps_to_last_item() {
        let mut s = state();
        s.picker.items = vec!["a".to_string(), "b".to_string()];
        handle_picker_key(&mut s, KeyCode::Down);
        assert_eq!(s.picker.selected, 1);
        handle_picker_key(&mut s, KeyCode::Down);
        assert_eq!(s.picker.selected, 1); // clamped, only 2 items
    }

    #[test]
    fn handle_picker_key_esc_cancels() {
        let mut s = state();
        assert_eq!(
            handle_picker_key(&mut s, KeyCode::Esc),
            PickerOutcome::Cancel
        );
    }

    #[test]
    fn handle_picker_key_enter_selects_typed_text_when_no_match() {
        let mut s = state();
        s.picker.filter = "typed-model".to_string();
        assert_eq!(
            handle_picker_key(&mut s, KeyCode::Enter),
            PickerOutcome::Select("typed-model".to_string())
        );
    }

    #[test]
    fn handle_picker_key_enter_selects_highlighted_row() {
        let mut s = state();
        s.picker.items = vec!["gpt-5.6-sol".to_string(), "gpt-5.6-codex".to_string()];
        s.picker.selected = 1;
        assert_eq!(
            handle_picker_key(&mut s, KeyCode::Enter),
            PickerOutcome::Select("gpt-5.6-codex".to_string())
        );
    }

    // -- handle_slash_command: persistence side effects -------------------

    fn temp_project_dir() -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("kode-tui-test-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn handle_slash_command_model_set_persists_and_updates_state() {
        let dir = temp_project_dir();
        let mut s = state();
        let mut cfg = KodeConfig::default();
        let (tx, _rx) = mpsc::unbounded_channel();

        handle_slash_command(
            &mut s,
            &dir,
            &mut cfg,
            &tx,
            SlashCommand::Model(Some("gpt-5.6-sol".to_string())),
        );

        assert_eq!(s.status.model, "gpt-5.6-sol");
        assert_eq!(cfg.model.model, "gpt-5.6-sol");
        assert!(s.transcript.iter().any(|l| l.text.contains("gpt-5.6-sol")));

        let reloaded = KodeConfig::load(&dir).unwrap();
        assert_eq!(reloaded.model.model, "gpt-5.6-sol");
    }

    #[test]
    fn handle_slash_command_effort_invalid_pushes_error_without_persisting() {
        let dir = temp_project_dir();
        let mut s = state();
        let mut cfg = KodeConfig::default();
        let (tx, _rx) = mpsc::unbounded_channel();

        handle_slash_command(
            &mut s,
            &dir,
            &mut cfg,
            &tx,
            SlashCommand::Effort("banana".to_string()),
        );

        assert_eq!(cfg.model.effort, "");
        assert!(
            s.transcript
                .iter()
                .any(|l| l.text.contains("invalid effort"))
        );
        assert!(!KodeConfig::config_path(&dir).exists());
    }

    #[test]
    fn handle_slash_command_effort_valid_persists() {
        let dir = temp_project_dir();
        let mut s = state();
        let mut cfg = KodeConfig::default();
        let (tx, _rx) = mpsc::unbounded_channel();

        handle_slash_command(
            &mut s,
            &dir,
            &mut cfg,
            &tx,
            SlashCommand::Effort("high".to_string()),
        );

        assert_eq!(s.status.effort, "high");
        assert_eq!(cfg.model.effort, "high");
        let reloaded = KodeConfig::load(&dir).unwrap();
        assert_eq!(reloaded.model.effort, "high");
    }

    #[test]
    fn handle_slash_command_help_lists_commands() {
        let dir = temp_project_dir();
        let mut s = state();
        let mut cfg = KodeConfig::default();
        let (tx, _rx) = mpsc::unbounded_channel();

        handle_slash_command(&mut s, &dir, &mut cfg, &tx, SlashCommand::Help);

        assert!(s.transcript.iter().any(|l| l.text.contains("/model")));
    }
}
