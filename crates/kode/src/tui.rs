mod markdown;
mod theme;

use std::collections::VecDeque;
use std::io::Stdout;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use futures::StreamExt;
use kode_core::CancellationToken;
use kode_core::config::{KodeConfig, PermissionMode};
use kode_core::event::{EventBus, KodeEvent, TaskStep};
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
    /// A verification step that passed.
    Verify,
    /// A verification step that failed.
    VerifyFail,
    /// A verification step that was skipped.
    VerifySkip,
    /// A progress/degradation note.
    Note,
    /// An agent-level error.
    Error,
    /// Echoed user input.
    User,
}

/// One line of transcript: its provenance gutter plus the rendered text.
/// `md_kind`/`spans` are `Some` for markdown-rendered Prose lines (see
/// `tui/markdown.rs`); `None` means legacy plain text — the gutter's own
/// text is drawn as-is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptLine {
    pub gutter: Gutter,
    pub text: String,
    pub md_kind: Option<markdown::MdKind>,
    pub spans: Option<Vec<(String, markdown::MdStyle)>>,
}

impl TranscriptLine {
    pub fn new(gutter: Gutter, text: impl Into<String>) -> Self {
        Self {
            gutter,
            text: text.into(),
            md_kind: None,
            spans: None,
        }
    }

    /// A markdown-rendered Prose line: `text` is kept as the plain
    /// fallback/search text, `kind`/`spans` drive styled rendering.
    pub fn markdown(
        gutter: Gutter,
        text: impl Into<String>,
        kind: markdown::MdKind,
        spans: Vec<(String, markdown::MdStyle)>,
    ) -> Self {
        Self {
            gutter,
            text: text.into(),
            md_kind: Some(kind),
            spans: Some(spans),
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

/// Lightweight mirror of `kode_verify::StepStatus`, minus the skip reason
/// text — the Ledger view only needs pass/fail/skip for its per-step glyph.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepStatusLite {
    Passed,
    Failed,
    Skipped,
}

/// Which real-event source a Ledger "WHY" line came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WhySource {
    Zindeks,
    Ingat,
}

/// The Knowledge Aperture's data — set when a `Knowledge` event arrives for
/// the current run, cleared once it contracts. `trigger_seen` flips true on
/// the first `ToolStarted`/`ModelToken` after it appeared; the aperture
/// only actually contracts once `aperture_should_collapse` (900ms floor)
/// says so.
#[derive(Debug, Clone)]
pub struct ApertureState {
    pub received_at: Instant,
    pub knowledge: KnowledgeState,
    pub trigger_seen: bool,
}

/// The Ledger view's (Ctrl+L) data — derived entirely from real events, no
/// invented captions. `steps` is always the fixed 4-step lifecycle in
/// order (Understand, Decide, Change, Verify).
#[derive(Debug, Clone)]
pub struct LedgerState {
    pub objective: String,
    pub steps: Vec<(TaskStep, bool)>,
    pub verify_steps: Vec<(String, StepStatusLite)>,
    pub last_change: Option<String>,
    pub why: Vec<(WhySource, String)>,
    apply_patch_calls: u32,
    write_file_calls: u32,
}

impl Default for LedgerState {
    fn default() -> Self {
        Self {
            objective: String::new(),
            steps: vec![
                (TaskStep::Understand, false),
                (TaskStep::Decide, false),
                (TaskStep::Change, false),
                (TaskStep::Verify, false),
            ],
            verify_steps: Vec::new(),
            last_change: None,
            why: Vec::new(),
            apply_patch_calls: 0,
            write_file_calls: 0,
        }
    }
}

impl LedgerState {
    fn new(objective: String) -> Self {
        Self {
            objective,
            ..Default::default()
        }
    }

    /// Records a successful `apply_patch`/`write_file` tool finish and
    /// recomputes `last_change`'s counter summary (e.g. `"apply_patch ×2 ·
    /// write_file ×1"`). No-op for any other tool name.
    fn record_change(&mut self, tool_name: &str) {
        match tool_name {
            "apply_patch" => self.apply_patch_calls += 1,
            "write_file" => self.write_file_calls += 1,
            _ => return,
        }
        let mut parts = Vec::new();
        if self.apply_patch_calls > 0 {
            parts.push(format!("apply_patch ×{}", self.apply_patch_calls));
        }
        if self.write_file_calls > 0 {
            parts.push(format!("write_file ×{}", self.write_file_calls));
        }
        self.last_change = Some(parts.join(" · "));
    }
}

/// A pending permission request awaiting a y/n answer from the user.
pub struct PermReq {
    pub summary: String,
    pub responder: oneshot::Sender<bool>,
}

/// Which catalog a `PickerState` is currently showing — drives what
/// `Enter`-ing a selection does (set the model vs. switch the provider).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PickerKind {
    #[default]
    Model,
    Provider,
}

/// State of the `/model`/`/provider` picker overlay. `items` holds the
/// fetched catalog (or is empty while loading / on fetch failure); `note`
/// carries a status line (loading / error) shown above the list.
#[derive(Debug, Clone, Default)]
pub struct PickerState {
    pub open: bool,
    pub kind: PickerKind,
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
    /// When the currently running tool started — drives the tool elapsed
    /// label.
    pub tool_started: Option<Instant>,
    /// The Knowledge Aperture's data, `Some` from the current run's
    /// `Knowledge` event until it contracts back to the normal band.
    pub aperture: Option<ApertureState>,
    /// Whether the Ledger view (Ctrl+L) is showing instead of the
    /// transcript.
    pub ledger_open: bool,
    /// The Ledger view's data, reset on every new task submission.
    pub ledger: LedgerState,
    /// True once the current run's `Decide` step has been marked done
    /// (first `ToolStarted` of the run). Reset on task submission.
    decide_marked_this_run: bool,
    /// Whether zindeks is enabled in config (`[zindeks].enabled`). Static
    /// config truth only — never probed at startup. Drives the idle
    /// empty-state's "code intelligence" line.
    pub zindeks_enabled: bool,
    /// Whether ingat is enabled in config (`[ingat].enabled`). Same
    /// static-truth rule as `zindeks_enabled`.
    pub ingat_enabled: bool,
    /// Auto mode (Shift+Tab): tools run without a permission prompt while
    /// on. `auto_flag` is the shared handle the permission handler reads
    /// from another task, so a toggle applies to in-flight runs too.
    pub auto_mode: bool,
    pub auto_flag: Arc<AtomicBool>,
    /// The most recently *completed* agent message's full text — what
    /// Ctrl+Y/`/copy` copy to the clipboard. Empty until the first message
    /// finishes.
    pub last_response: String,
    /// Accumulates flushed prose chunks for the run currently in flight;
    /// swapped into `last_response` on `TaskFinished`/`AgentError`.
    response_buf: String,
    /// Per-message ``` fence state for markdown rendering of flushed Prose
    /// lines; reset on every new task submission.
    md_in_code_block: bool,
    /// Highlighted row in the slash-command hint menu.
    pub slash_selected: usize,
    /// Completed turns of the active session — sent as model history.
    pub history: Vec<crate::session::Turn>,
    /// Active session file id; created lazily on first completed task.
    pub session_id: Option<String>,
    /// Task text of the in-flight run; consumed when TaskFinished arrives.
    pub pending_task: Option<String>,
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
            tool_started: None,
            aperture: None,
            ledger_open: false,
            ledger: LedgerState::default(),
            decide_marked_this_run: false,
            zindeks_enabled: true,
            ingat_enabled: true,
            auto_mode: false,
            auto_flag: Arc::new(AtomicBool::new(false)),
            last_response: String::new(),
            response_buf: String::new(),
            md_in_code_block: false,
            slash_selected: 0,
            history: Vec::new(),
            session_id: None,
            pending_task: None,
        }
    }

    pub fn push_permission(&mut self, req: PermReq) {
        self.pending.push_back(req);
    }

    pub fn pop_permission(&mut self) -> Option<PermReq> {
        self.pending.pop_front()
    }

    /// Resets per-run state for a freshly submitted task: the Ledger
    /// (objective + steps), the Decide-derivation flag, and any leftover
    /// Aperture from a prior run. Pure — the caller still owns emitting the
    /// actual task to the pipeline.
    pub fn start_new_task(&mut self, task: &str) {
        self.ledger = LedgerState::new(ledger_objective(task));
        self.decide_marked_this_run = false;
        self.aperture = None;
        self.tool_started = None;
        self.response_buf.clear();
        self.md_in_code_block = false;
        self.pending_task = Some(task.to_string());
    }
}

/// Persists the in-flight task (if any) as a completed `session::Turn`: both
/// to disk (creating the session lazily on first write) and into
/// `state.history` for the next task's model replay. No-op when
/// `pending_task` is `None` (nothing was in flight — e.g. a stray event).
/// Store I/O failures are surfaced as transcript Notes, never fatal.
fn record_completed_turn(
    state: &mut AppState,
    cwd: &Path,
    provider: &str,
    model: &str,
    tool_calls: u32,
) {
    if let Some(task_text) = state.pending_task.take() {
        let (_, ts) = crate::session::now_utc_stamp();
        let turn = crate::session::Turn {
            ts,
            task: task_text,
            response: state.last_response.clone(),
            tool_calls,
        };
        let id = match state.session_id.clone() {
            Some(id) => Some(id),
            None => match crate::session::create(cwd, provider, model) {
                Ok(id) => {
                    state.session_id = Some(id.clone());
                    Some(id)
                }
                Err(e) => {
                    state.transcript.push(TranscriptLine::new(
                        Gutter::Note,
                        format!("session store unavailable: {e}"),
                    ));
                    None
                }
            },
        };
        if let Some(id) = id
            && let Err(e) = crate::session::append_turn(cwd, &id, &turn)
        {
            state.transcript.push(TranscriptLine::new(
                Gutter::Note,
                format!("session append failed (non-fatal): {e}"),
            ));
        }
        state.history.push(turn);
    }
}

/// First line of `task`, truncated to 70 chars (char-safe) — the Ledger
/// view's OBJECTIVE text.
fn ledger_objective(task: &str) -> String {
    let first_line = task.lines().next().unwrap_or("");
    truncate_chars(first_line, 70)
}

/// Truncates `s` to at most `max` chars, appending `…` when truncated.
/// Char-safe (splits on `char_indices`, never mid-codepoint).
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut truncated: String = s.chars().take(max).collect();
        truncated.push('…');
        truncated
    }
}

/// Applies one `KodeEvent` to `state`. Any accumulated `current_stream` text
/// is flushed into the transcript before non-token events are processed, so
/// the transcript always reads as a sequence of complete lines.
pub fn apply_event(state: &mut AppState, ev: KodeEvent) {
    if !matches!(ev, KodeEvent::ModelToken { .. }) && !state.current_stream.is_empty() {
        let text = std::mem::take(&mut state.current_stream);
        state.response_buf.push_str(&text);
        for line in text.split('\n') {
            if line.is_empty() {
                state.transcript.push(TranscriptLine::new(Gutter::None, ""));
            } else {
                let rendered = markdown::render_line(line, &mut state.md_in_code_block);
                if rendered.kind == markdown::MdKind::Heading
                    && !matches!(state.transcript.last(), Some(l) if l.gutter == Gutter::None)
                {
                    state.transcript.push(TranscriptLine::new(Gutter::None, ""));
                }
                state.transcript.push(TranscriptLine::markdown(
                    Gutter::Prose,
                    line,
                    rendered.kind,
                    rendered.spans,
                ));
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
            if let Some(ap) = &mut state.aperture {
                ap.trigger_seen = true;
            }
        }
        KodeEvent::ToolRequested { .. } => {}
        KodeEvent::ToolStarted { name } => {
            state
                .transcript
                .push(TranscriptLine::new(Gutter::Tool, name.clone()));
            state.status.tools_used += 1;
            state.status.state = RunState::Tool;
            state.current_tool = Some(name);
            state.tool_started = Some(Instant::now());
            if let Some(ap) = &mut state.aperture {
                ap.trigger_seen = true;
            }
            if !state.decide_marked_this_run {
                state.decide_marked_this_run = true;
                if let Some(entry) = state
                    .ledger
                    .steps
                    .iter_mut()
                    .find(|(step, _)| *step == TaskStep::Decide)
                {
                    entry.1 = true;
                }
            }
        }
        KodeEvent::ToolFinished { name, ok } => {
            if !ok {
                state.transcript.push(TranscriptLine::new(
                    Gutter::ToolFail,
                    format!("{name} failed"),
                ));
            } else {
                state.ledger.record_change(&name);
            }
            state.current_tool = None;
            state.tool_started = None;
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
            if !state.response_buf.is_empty() {
                state.last_response = std::mem::take(&mut state.response_buf);
            }
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
            if !state.response_buf.is_empty() {
                state.last_response = std::mem::take(&mut state.response_buf);
            }
        }
        KodeEvent::Knowledge {
            zindeks,
            ingat,
            git,
            context_tokens,
            budget_tokens,
        } => {
            let ks = KnowledgeState {
                zindeks,
                ingat,
                git,
                context_tokens,
                budget_tokens,
            };
            state.ledger.why = ledger_why_from(&ks);
            // The Aperture is a code-intelligence moment — it opens only
            // when a zindeks or ingat engine actually contributed evidence.
            // Git-only compilations still populate the Knowledge Band, but
            // git impact alone isn't "intelligence made visible" (DESIGN.md:
            // absent when engines are absent).
            if !ks.zindeks.is_empty() || !ks.ingat.is_empty() {
                state.aperture = Some(ApertureState {
                    received_at: Instant::now(),
                    knowledge: ks.clone(),
                    trigger_seen: false,
                });
            }
            state.knowledge = Some(ks);
        }
        KodeEvent::VerifyStep {
            name,
            passed,
            skipped,
            duration_ms,
        } => {
            let dur = duration_ms as f64 / 1000.0;
            let (gutter, text, status) = if skipped {
                (
                    Gutter::VerifySkip,
                    format!("{name} · {dur:.1}s – (skipped)"),
                    StepStatusLite::Skipped,
                )
            } else if passed {
                (
                    Gutter::Verify,
                    format!("{name} · {dur:.1}s ✓"),
                    StepStatusLite::Passed,
                )
            } else {
                (
                    Gutter::VerifyFail,
                    format!("{name} · {dur:.1}s ×"),
                    StepStatusLite::Failed,
                )
            };
            state.transcript.push(TranscriptLine::new(gutter, text));
            state.ledger.verify_steps.push((name, status));
        }
        KodeEvent::TaskProgress { step, done } => {
            if let Some(entry) = state.ledger.steps.iter_mut().find(|(s, _)| *s == step) {
                entry.1 = done;
            }
        }
    }
}

/// Builds the Ledger view's WHY lines from a Knowledge digest: the first
/// zindeks fact and the first ingat memory, when present. No invented
/// captions — real event data only.
fn ledger_why_from(ks: &KnowledgeState) -> Vec<(WhySource, String)> {
    let mut why = Vec::new();
    if let Some(z) = ks.zindeks.first() {
        why.push((WhySource::Zindeks, z.clone()));
    }
    if let Some(i) = ks.ingat.first() {
        why.push((WhySource::Ingat, i.clone()));
    }
    why
}

/// A parsed `/`-prefixed slash command.
#[derive(Debug, Clone, PartialEq)]
pub enum SlashCommand {
    /// `/model` (open picker) or `/model <name>` (set directly).
    Model(Option<String>),
    /// `/effort <value>`.
    Effort(String),
    /// `/provider` (open picker) or `/provider <name>` (set directly).
    Provider(Option<String>),
    /// `/copy` — copies `last_response` to the clipboard.
    Copy,
    Help,
    Unknown(String),
}

/// The providers `/provider` accepts, in picker display order.
pub const VALID_PROVIDERS: &[&str] = &[
    "openai",
    "codex",
    "opencode-go",
    "opencode",
    "kilo",
    "lmstudio",
];

/// Commands listed in the `/` hint menu: (name, one-line description).
pub const SLASH_COMMANDS: &[(&str, &str)] = &[
    ("/model", "pick or set model"),
    ("/effort", "set reasoning effort (minimal|low|medium|high)"),
    ("/provider", "pick provider"),
    ("/copy", "copy last response to clipboard"),
    ("/help", "list commands + shortcuts"),
];

/// Hint-menu rows for the current input. Non-empty only while the input is a
/// bare `/`-prefixed token with no whitespace (once arguments start, the menu
/// hides). Bare `/` lists every command; `/mo` narrows by prefix.
pub fn slash_hint_items(input: &str) -> Vec<(&'static str, &'static str)> {
    if !input.starts_with('/') || input.contains(char::is_whitespace) {
        return Vec::new();
    }
    SLASH_COMMANDS
        .iter()
        .copied()
        .filter(|(name, _)| name.starts_with(input))
        .collect()
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
        "/provider" => SlashCommand::Provider(if rest.is_empty() {
            None
        } else {
            Some(rest.to_string())
        }),
        "/copy" => SlashCommand::Copy,
        "/help" => SlashCommand::Help,
        other => SlashCommand::Unknown(other.to_string()),
    })
}

/// Auth-state annotation appended to a provider's name in the `/provider`
/// picker. `" ✓ logged in"` when credentials for that provider are on disk
/// (or in the environment for `openai`); `" (local)"` always for `lmstudio`
/// (no login needed — it's a local server); `""` otherwise. Pure — callers
/// gather `codex_auth`/`opencode_keys`/`env_key` from disk/env once per
/// picker open.
pub fn provider_auth_state(
    provider: &str,
    codex_auth: bool,
    opencode_keys: &[String],
    env_key: bool,
) -> &'static str {
    match provider {
        "codex" => {
            if codex_auth {
                " ✓ logged in"
            } else {
                ""
            }
        }
        "opencode-go" | "opencode" | "kilo" => {
            if opencode_keys.iter().any(|k| k == provider) {
                " ✓ logged in"
            } else {
                ""
            }
        }
        "openai" => {
            if env_key {
                " ✓ logged in"
            } else {
                ""
            }
        }
        "lmstudio" => " (local)",
        _ => "",
    }
}

/// Decides the startup hint (if any) shown once at TUI launch: a nudge to
/// switch providers when the config is still on the `openai` default, no
/// model has been explicitly chosen, no OpenAI credentials are available,
/// but Kode's own credential store has something usable. Fires at most one
/// hint — codex takes priority over opencode. Pure so the decision is
/// unit-testable without touching the filesystem/env.
fn startup_hint(
    provider: &str,
    model_set: bool,
    env_key: bool,
    codex_auth: bool,
    opencode_any: bool,
) -> Option<&'static str> {
    if provider != "openai" || model_set || env_key {
        return None;
    }
    if codex_auth {
        Some("logged in via codex — run /provider codex to use it")
    } else if opencode_any {
        Some("opencode key found — run /provider opencode-go")
    } else {
        None
    }
}

/// Whether Kode's own codex OAuth credentials file exists (`kode auth login
/// codex`). Used only to power the `/provider` picker annotation and the
/// startup hint — not a validity check of the tokens inside.
fn codex_auth_exists() -> bool {
    kode_model::codex::default_auth_path()
        .map(|p| p.exists())
        .unwrap_or(false)
}

/// Provider ids with a stored API key in Kode's opencode-family auth store
/// (`~/.kode/auth/opencode.json`). Empty when the file is missing/invalid.
fn opencode_key_ids() -> Vec<String> {
    let Some(path) = kode_model::opencode::default_auth_path() else {
        return Vec::new();
    };
    let Ok(content) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    serde_json::from_str::<serde_json::Value>(&content)
        .ok()
        .and_then(|v| v.as_object().map(|o| o.keys().cloned().collect()))
        .unwrap_or_default()
}

/// True when an OpenAI API key is available via environment variable
/// (`OPENAI_API_KEY` or `KODE_API_KEY`) — the credential path the `openai`
/// provider actually uses.
fn openai_env_key_present() -> bool {
    std::env::var("OPENAI_API_KEY").is_ok() || std::env::var("KODE_API_KEY").is_ok()
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
    state.picker.kind = PickerKind::Model;
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

/// Opens the `/provider` picker: a static list of [`VALID_PROVIDERS`], each
/// annotated with its auth state via [`provider_auth_state`]. Synchronous —
/// no catalog fetch, just local disk/env reads.
fn open_provider_picker(state: &mut AppState) {
    state.picker.open = true;
    state.picker.kind = PickerKind::Provider;
    state.picker.filter.clear();
    state.picker.selected = 0;
    state.picker.note = None;

    let codex_auth = codex_auth_exists();
    let opencode_keys = opencode_key_ids();
    let env_key = openai_env_key_present();
    state.picker.items = VALID_PROVIDERS
        .iter()
        .map(|p| {
            format!(
                "{p}{}",
                provider_auth_state(p, codex_auth, &opencode_keys, env_key)
            )
        })
        .collect();
}

/// Applies a validated `/provider` switch: persists the new provider
/// (clearing `model` — it's very unlikely to be valid across providers),
/// updates in-memory state, and auto-opens the model picker for the new
/// provider so the user isn't left at "(no model)".
fn apply_provider_selection(
    state: &mut AppState,
    cwd: &Path,
    config: &mut KodeConfig,
    picker_tx: &mpsc::UnboundedSender<PickerLoaded>,
    provider: &str,
) {
    state.status.provider = provider.to_string();
    state.status.model = String::new();
    config.model.provider = provider.to_string();
    config.model.model = String::new();
    let _ = KodeConfig::update_model_config(cwd, Some(provider), Some(""), None);
    state.transcript.push(TranscriptLine::new(
        Gutter::Note,
        format!("provider set: {provider}"),
    ));
    open_picker(state, provider.to_string(), picker_tx);
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
        SlashCommand::Provider(None) => {
            open_provider_picker(state);
        }
        SlashCommand::Provider(Some(name)) => {
            if VALID_PROVIDERS.contains(&name.as_str()) {
                apply_provider_selection(state, cwd, config, picker_tx, &name);
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
        SlashCommand::Copy => perform_copy(state),
        SlashCommand::Help => {
            state.transcript.push(TranscriptLine::new(
                Gutter::Note,
                "commands: /model [name], /effort <minimal|low|medium|high|xhigh>, \
                 /provider [name], /copy, /help · shift+tab toggles auto mode (tools run \
                 without asking) · ctrl+y copies the last response · text selection works \
                 natively (no mouse capture) · Ctrl+K toggles the Knowledge Band, Ctrl+L \
                 opens the Ledger, Esc closes the Ledger or cancels the run",
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
/// idle) or the process is otherwise terminated. `_continue_` resumes the
/// latest session (wired in a later task; accepted now so call sites are
/// stable).
pub async fn run(cwd: &Path, cancel: CancellationToken, _continue_: bool) -> anyhow::Result<()> {
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
    state.zindeks_enabled = config.zindeks.enabled;
    state.ingat_enabled = config.ingat.enabled;

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

    let (perm_tx, mut perm_rx) = mpsc::unbounded_channel::<(String, oneshot::Sender<bool>)>();
    let handler: Arc<dyn PermissionHandler> =
        Arc::new(TuiPermission::new(perm_tx, state.auto_flag.clone()));

    let (picker_tx, mut picker_rx) = mpsc::unbounded_channel::<PickerLoaded>();

    let events = EventBus::new(256);
    let mut event_rx = events.subscribe();

    let mut key_events = EventStream::new();
    let mut current_cancel: Option<CancellationToken> = None;
    let mut aperture_tick = tokio::time::interval(Duration::from_millis(100));

    terminal.draw(|f| draw(f, &state))?;

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
                                    }
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
                                let mut input = std::mem::take(&mut state.input);
                                let hints = slash_hint_items(&input);
                                if !hints.is_empty() {
                                    // Enter on a hint row completes to the highlighted command.
                                    input = hints[state.slash_selected.min(hints.len() - 1)].0.to_string();
                                    state.slash_selected = 0;
                                }
                                if let Some(cmd) = parse_slash_command(&input) {
                                    handle_slash_command(&mut state, cwd, &mut config, &picker_tx, cmd);
                                } else if state.status.model.is_empty() {
                                    state.transcript.push(TranscriptLine::new(Gutter::Note, "pick a model first"));
                                    open_picker(&mut state, config.model.provider.clone(), &picker_tx);
                                } else {
                                    let task = input;
                                    state.start_new_task(&task);
                                    state
                                        .transcript
                                        .push(TranscriptLine::new(Gutter::User, task.clone()));
                                    let child = cancel.child_token();
                                    current_cancel = Some(child.clone());
                                    state.running = true;
                                    state.status.state = RunState::Thinking;

                                    let task_events = events.clone();
                                    let task_cwd = cwd.to_path_buf();
                                    let mut task_config = config.clone();
                                    if state.auto_mode {
                                        // Belt and braces alongside TuiPermission's
                                        // `auto` flag: skip the Ask path entirely for
                                        // runs started while auto mode is on.
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
                                    tokio::spawn(async move {
                                        if let Err(err) = pipeline::run_task(
                                            &task,
                                            &task_cwd,
                                            &task_config,
                                            task_events.clone(),
                                            task_handler,
                                            child,
                                            &task_history,
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

            _ = aperture_tick.tick() => {
                if let Some(ap) = &state.aperture
                    && aperture_should_collapse(ap.received_at, Instant::now(), ap.trigger_seen)
                {
                    state.aperture = None;
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

    if modifiers.contains(KeyModifiers::CONTROL) && code == KeyCode::Char('l') {
        state.ledger_open = !state.ledger_open;
        return false;
    }

    if modifiers.contains(KeyModifiers::CONTROL) && code == KeyCode::Char('y') {
        perform_copy(state);
        return false;
    }

    if code == KeyCode::BackTab {
        toggle_auto_mode(state);
        return false;
    }

    let hint_count = if state.pending.is_empty() && !state.picker.open {
        slash_hint_items(&state.input).len()
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
            let items = slash_hint_items(&state.input);
            let (name, _) = items[state.slash_selected.min(items.len() - 1)];
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

/// Toggles auto mode (Shift+Tab): flips both the UI-visible `auto_mode`
/// bool and the `auto_flag` the permission handler reads from the task
/// task, and leaves a transcript Note describing the new state.
fn toggle_auto_mode(state: &mut AppState) {
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
fn perform_copy(state: &mut AppState) {
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
fn copy_to_clipboard(text: &str) -> Result<usize, ()> {
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

/// True when the transcript area should render the idle empty-state block
/// (version/tagline, engine status, input nudge) instead of the normal
/// transcript. Per `DESIGN.md`, this is the calm-instrument first-run
/// surface — it stays true while nothing but startup `Note` hints (model
/// unset, provider suggestion) have landed, and goes away for good once
/// any real activity (user input, prose, tool, verify, error) appears.
/// Never true while a task is running.
pub fn show_empty_state(transcript: &[TranscriptLine], running: bool) -> bool {
    !running && transcript.iter().all(|l| l.gutter == Gutter::Note)
}

/// The idle empty-state's per-engine status word — derived from static
/// config truth plus session state only, never by probing the engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineStatus {
    /// `[zindeks]`/`[ingat]` `enabled = false` in config.
    Disabled,
    /// Enabled, and at least one `Knowledge` event has surfaced data from
    /// this source this session.
    Ready,
    /// Enabled, but no `Knowledge` event has surfaced data from this
    /// source yet.
    AvailableAfterFirstTask,
}

/// Decides an [`EngineStatus`] from config's `enabled` flag and whether
/// this source has produced data in a `Knowledge` event yet this session.
pub fn engine_status(enabled: bool, source_seen: bool) -> EngineStatus {
    if !enabled {
        EngineStatus::Disabled
    } else if source_seen {
        EngineStatus::Ready
    } else {
        EngineStatus::AvailableAfterFirstTask
    }
}

/// The input line's right-aligned suffix: per-source context counts once a
/// `Knowledge` event has arrived this session, else the `/help` hint. Pure
/// decision — sizing and rendering both derive from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputSuffix {
    Counts { z: usize, i: usize, g: usize },
    Help,
}

impl InputSuffix {
    /// Plain-text rendering, used to size the row for right-alignment.
    fn plain_text(&self) -> String {
        match self {
            InputSuffix::Counts { z, i, g } => format!("ctx Z:{z} I:{i} G:{g}"),
            InputSuffix::Help => "/help".to_string(),
        }
    }

    /// Styled spans, colored per source (`ctx` label DIM, counts in source
    /// colors) or a plain DIM `/help`.
    fn spans(&self) -> Vec<Span<'static>> {
        match self {
            InputSuffix::Counts { z, i, g } => vec![
                Span::styled("ctx ", Style::default().fg(theme::DIM)),
                Span::styled(format!("Z:{z}"), Style::default().fg(theme::Z)),
                Span::raw(" "),
                Span::styled(format!("I:{i}"), Style::default().fg(theme::I)),
                Span::raw(" "),
                Span::styled(format!("G:{g}"), Style::default().fg(theme::G)),
            ],
            InputSuffix::Help => vec![Span::styled("/help", Style::default().fg(theme::DIM))],
        }
    }
}

/// Decides the input line's suffix from the session's last Knowledge
/// digest (`None` until the first context compilation of the session).
pub fn input_suffix(knowledge: Option<&KnowledgeState>) -> InputSuffix {
    match knowledge {
        Some(ks) => InputSuffix::Counts {
            z: ks.zindeks.len(),
            i: ks.ingat.len(),
            g: ks.git.len(),
        },
        None => InputSuffix::Help,
    }
}

/// One idle empty-state engine-status line: ` {label}: {status word}`.
fn engine_status_line(
    label: &str,
    enabled: bool,
    source_seen: bool,
    ready_color: Color,
) -> Line<'static> {
    let (text, style) = match engine_status(enabled, source_seen) {
        EngineStatus::Disabled => ("disabled".to_string(), Style::default().fg(theme::DIM)),
        EngineStatus::Ready => ("ready".to_string(), Style::default().fg(ready_color)),
        EngineStatus::AvailableAfterFirstTask => (
            "available after first task".to_string(),
            Style::default().fg(theme::DIM),
        ),
    };
    Line::from(vec![
        Span::styled(format!(" {label}: "), Style::default().fg(theme::MUTED)),
        Span::styled(text, style),
    ])
}

/// Builds the idle empty-state block: version/tagline, per-engine status
/// (zindeks/ingat), then the input nudge and command list. Top-left
/// anchored, one blank row down — never vertically centered, per
/// `DESIGN.md`'s calm-instrument direction.
fn empty_state_lines(state: &AppState) -> Vec<Line<'static>> {
    let zindeks_seen = state
        .knowledge
        .as_ref()
        .is_some_and(|k| !k.zindeks.is_empty());
    let ingat_seen = state
        .knowledge
        .as_ref()
        .is_some_and(|k| !k.ingat.is_empty());
    vec![
        Line::default(),
        Line::from(vec![
            Span::styled(" kode", Style::default().add_modifier(Modifier::BOLD)),
            Span::styled(
                format!(" v{} — calm instrument", env!("CARGO_PKG_VERSION")),
                Style::default().fg(theme::MUTED),
            ),
        ]),
        engine_status_line(
            "code intelligence",
            state.zindeks_enabled,
            zindeks_seen,
            theme::Z,
        ),
        engine_status_line(
            "engineering memory",
            state.ingat_enabled,
            ingat_seen,
            theme::I,
        ),
        Line::default(),
        Line::from(vec![
            Span::styled(" › ", Style::default().fg(theme::MUTED)),
            Span::styled(
                "type a task and press enter",
                Style::default().fg(theme::MUTED),
            ),
        ]),
        Line::from(Span::styled(
            " /provider · /model · /effort · /help · ctrl+k band · ctrl+l ledger",
            Style::default().fg(theme::DIM),
        )),
    ]
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

/// Aperture's contraction decision: it never collapses before a
/// `ToolStarted`/`ModelToken` trigger has been seen, and — per `DESIGN.md`
/// motion rules — never earlier than 900ms after it appeared, so it's
/// perceivable even when the trigger fires almost instantly. Pure, so the
/// tick loop's timing behavior is unit-testable without a real clock race.
fn aperture_should_collapse(received: Instant, now: Instant, trigger_seen: bool) -> bool {
    trigger_seen && now.saturating_duration_since(received) >= Duration::from_millis(900)
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
        Gutter::VerifyFail => ("V ", theme::ERR),
        Gutter::VerifySkip => ("V ", theme::DIM),
        Gutter::Note => ("· ", theme::DIM),
        Gutter::Error => ("× ", theme::ERR),
        Gutter::User => ("› ", theme::MUTED),
    }
}

/// Maps a markdown inline style onto a ratatui `Style`, within the existing
/// palette per `DESIGN.md` — no new colors, only bold/dim. Color is
/// provenance, never decoration, so inline code is muted rather than tinted
/// with a source color it doesn't carry.
fn md_span_style(style: &markdown::MdStyle) -> Style {
    match style {
        markdown::MdStyle::Plain => Style::default(),
        markdown::MdStyle::Bold => Style::default().add_modifier(Modifier::BOLD),
        markdown::MdStyle::Italic => Style::default(),
        markdown::MdStyle::InlineCode => Style::default().fg(theme::MUTED),
    }
}

/// Renders one transcript line as a gutter span + text span(s). Long lines
/// wrap via `Paragraph`'s own word-wrap without gutter-aligned
/// continuation (acceptable ceiling for this phase). Markdown-rendered
/// Prose lines (`md_kind`/`spans` both `Some`) render their styled spans;
/// everything else falls back to the legacy plain-text span.
fn transcript_line_to_ratatui(line: &TranscriptLine) -> Line<'static> {
    let (prefix, color) = gutter_prefix(&line.gutter);
    let mut prefix_style = Style::default().fg(color);
    // Source-letter glyphs (`T`/`V`) are bold+colored per DESIGN.md's glyph
    // vocabulary; the plain `│` prose bar and other non-letter glyphs stay
    // unbolded.
    if matches!(
        line.gutter,
        Gutter::Tool | Gutter::ToolFail | Gutter::Verify | Gutter::VerifyFail | Gutter::VerifySkip
    ) {
        prefix_style = prefix_style.add_modifier(Modifier::BOLD);
    }
    let mut spans = vec![Span::styled(prefix, prefix_style)];

    match (&line.md_kind, &line.spans) {
        (Some(markdown::MdKind::CodeFence), _) => {
            spans.push(Span::styled(
                "\u{2504}\u{2504}".to_string(),
                Style::default().fg(theme::DIM),
            ));
        }
        (Some(markdown::MdKind::Code), Some(md_spans)) => {
            let text: String = md_spans.iter().map(|(t, _)| t.as_str()).collect();
            spans.push(Span::raw(text));
        }
        (Some(markdown::MdKind::Bullet), Some(md_spans)) => {
            for (i, (text, style)) in md_spans.iter().enumerate() {
                if i == 0 {
                    spans.push(Span::styled(text.clone(), Style::default().fg(theme::DIM)));
                } else {
                    spans.push(Span::styled(text.clone(), md_span_style(style)));
                }
            }
        }
        (Some(markdown::MdKind::Heading), Some(md_spans))
        | (Some(markdown::MdKind::Plain), Some(md_spans)) => {
            for (text, style) in md_spans {
                spans.push(Span::styled(text.clone(), md_span_style(style)));
            }
        }
        _ => {
            spans.push(Span::raw(line.text.clone()));
        }
    }
    Line::from(spans)
}

/// Builds the breadcrumb row: `kode  {repo} · {branch} · {provider}/{model}
/// · effort:{e} · ctx ▓▓░░ {used}/{budget}`. `kode` renders dim/lowercase,
/// the rest normal; when no model is selected, a dim ` — /model` nudge is
/// appended after the provider/model cell. The context meter is omitted
/// until the first `Knowledge` event of the session.
fn breadcrumb_line(state: &AppState) -> Line<'static> {
    let branch = state.branch.clone().unwrap_or_else(|| "no git".to_string());
    let effort = if state.status.effort.is_empty() {
        "-".to_string()
    } else {
        state.status.effort.clone()
    };
    let model_set = !state.status.model.is_empty();
    let model = if model_set {
        state.status.model.clone()
    } else {
        "(no model)".to_string()
    };
    let mut spans = vec![
        Span::styled(" kode", Style::default().fg(theme::DIM)),
        Span::raw(format!(
            "  {} · {branch} · {}/{model} · effort:{effort}",
            state.repo_dir, state.status.provider
        )),
    ];
    if !model_set {
        spans.push(Span::styled(" — /model", Style::default().fg(theme::DIM)));
    }
    if state.auto_mode {
        spans.push(Span::styled(
            " · auto",
            Style::default().fg(theme::T).add_modifier(Modifier::BOLD),
        ));
    }
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
/// rule line, which needs the render-time area width). Bounded to at most 3
/// rows — one per source (`Z`, `I`, `G`) — showing only the first fact from
/// each; a dim ` +N more` suffix marks additional facts the source holds.
/// Only the leading source glyph is bold+colored; the fact text itself
/// renders in its normal weight (glyph-only bold, per `DESIGN.md`). Sources
/// with empty vecs render nothing.
fn knowledge_band_lines(ks: &KnowledgeState) -> Vec<Line<'static>> {
    let mut lines: Vec<Line> = Vec::new();

    if let Some(first) = ks.zindeks.first() {
        let mut spans = vec![
            Span::raw(" KNOWS  "),
            Span::styled(
                "Z ",
                Style::default().fg(theme::Z).add_modifier(Modifier::BOLD),
            ),
            Span::raw(first.clone()),
        ];
        if ks.zindeks.len() > 1 {
            spans.push(Span::styled(
                format!(" +{} more", ks.zindeks.len() - 1),
                Style::default().fg(theme::DIM),
            ));
        }
        lines.push(Line::from(spans));
    }

    if let Some(first) = ks.ingat.first() {
        let mut spans = vec![
            Span::raw(" KNOWS  "),
            Span::styled(
                "I ",
                Style::default().fg(theme::I).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("\u{201c}{first}\u{201d}"),
                Style::default().fg(theme::I).add_modifier(Modifier::ITALIC),
            ),
        ];
        if ks.ingat.len() > 1 {
            spans.push(Span::styled(
                format!(" +{} more", ks.ingat.len() - 1),
                Style::default().fg(theme::DIM),
            ));
        }
        lines.push(Line::from(spans));
    }

    if let Some(git_line) = ks.git.first() {
        let mut spans = vec![
            Span::raw(" KNOWS  "),
            Span::styled(
                "G ",
                Style::default().fg(theme::G).add_modifier(Modifier::BOLD),
            ),
            Span::raw(git_line.clone()),
        ];
        if ks.git.len() > 1 {
            spans.push(Span::styled(
                format!(" +{} more", ks.git.len() - 1),
                Style::default().fg(theme::DIM),
            ));
        }
        lines.push(Line::from(spans));
    }

    lines
}

/// Builds the Knowledge Aperture's content lines (not including the
/// trailing rule): a bold header, a request tree of up to the first 2
/// zindeks facts + first ingat memory + first git line (tree connectors
/// `─┬─`/`├─`/`└─`, last present row gets `└─`), then a context summary
/// row. Rows are skipped for empty sources — never a decorative fake.
fn aperture_lines(ks: &KnowledgeState) -> Vec<Line<'static>> {
    let mut rows: Vec<(&'static str, Option<&'static str>, String, Style)> = Vec::new();
    for z in ks.zindeks.iter().take(2) {
        rows.push(("Z", None, z.clone(), Style::default().fg(theme::Z)));
    }
    if let Some(entry) = ks.ingat.first() {
        rows.push((
            "I",
            Some("recalled: "),
            format!("\u{201c}{entry}\u{201d}"),
            Style::default().fg(theme::I).add_modifier(Modifier::ITALIC),
        ));
    }
    if let Some(git_line) = ks.git.first() {
        rows.push(("G", None, git_line.clone(), Style::default().fg(theme::G)));
    }

    let mut lines = vec![Line::from(Span::styled(
        " KNOWLEDGE APERTURE",
        Style::default()
            .fg(theme::MUTED)
            .add_modifier(Modifier::BOLD),
    ))];

    let last = rows.len().saturating_sub(1);
    for (idx, (src, label, text, style)) in rows.into_iter().enumerate() {
        let (lead, connector) = if idx == 0 {
            (" request ", "─┬─ ")
        } else if idx == last {
            ("          ", "└─ ")
        } else {
            ("          ", "├─ ")
        };
        let mut row_spans = vec![
            Span::styled(lead, Style::default().fg(theme::MUTED)),
            Span::styled(connector, Style::default().fg(theme::DIM)),
            Span::styled(format!("{src} "), style.add_modifier(Modifier::BOLD)),
        ];
        if let Some(label) = label {
            row_spans.push(Span::styled(label, Style::default().fg(theme::MUTED)));
        }
        row_spans.push(Span::styled(text, style));
        lines.push(Line::from(row_spans));
    }

    lines.push(Line::from(vec![
        Span::styled(" context  ", Style::default().fg(theme::MUTED)),
        Span::raw(format!(
            "{} of {} tokens · Z:{} I:{} G:{}",
            format_k(ks.context_tokens),
            format_k(ks.budget_tokens),
            ks.zindeks.len(),
            ks.ingat.len(),
            ks.git.len(),
        )),
    ]));

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
    let band_lines = if state.ledger_open {
        // The Ledger view replaces the band + transcript area entirely.
        None
    } else if let Some(ap) = &state.aperture {
        Some(aperture_lines(&ap.knowledge))
    } else if knowledge_band_visible(state) {
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
    let hint_items = if !state.picker.open && state.pending.is_empty() && !state.ledger_open {
        slash_hint_items(&state.input)
    } else {
        Vec::new()
    };
    if !hint_items.is_empty() {
        constraints.push(Constraint::Length(hint_items.len() as u16));
    }
    constraints.push(Constraint::Length(2)); // input: rule + line, no box

    let areas = Layout::vertical(constraints).split(f.area());
    let mut idx = 0;

    f.render_widget(Paragraph::new(breadcrumb_line(state)), areas[idx]);
    idx += 1;

    if let Some(lines) = band_lines {
        draw_knowledge_band(f, areas[idx], lines);
        idx += 1;
    }

    if state.ledger_open {
        draw_ledger(f, areas[idx], &state.ledger);
    } else {
        let mut text_lines: Vec<Line> = if show_empty_state(&state.transcript, state.running) {
            let mut lines = empty_state_lines(state);
            lines.extend(state.transcript.iter().map(transcript_line_to_ratatui));
            lines
        } else {
            state
                .transcript
                .iter()
                .map(transcript_line_to_ratatui)
                .collect()
        };
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
                Some(tool) => {
                    let tool_secs = state
                        .tool_started
                        .map(|t| t.elapsed().as_secs_f64())
                        .unwrap_or(secs);
                    format!("▸ {tool} · {tool_secs:.1}s")
                }
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
    }
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
                Style::default().fg(theme::T).add_modifier(Modifier::BOLD),
            )),
            areas[idx],
        );
        idx += 1;
        areas[idx]
    } else {
        areas[idx]
    };

    let input_area = if hint_items.is_empty() {
        next_area
    } else {
        f.render_widget(
            Paragraph::new(slash_hint_lines(&hint_items, state.slash_selected)),
            next_area,
        );
        idx += 1;
        areas[idx]
    };

    draw_input(f, input_area, state);

    if state.picker.open {
        draw_picker(f, &state.picker);
    }
}

/// Hint-menu lines: highlighted row gets a `›` marker and bold name; others
/// indent. Descriptions render muted. No borders — DESIGN.md overlay rules.
fn slash_hint_lines(items: &[(&'static str, &'static str)], selected: usize) -> Vec<Line<'static>> {
    let name_w = items
        .iter()
        .map(|(n, _)| n.chars().count())
        .max()
        .unwrap_or(0);
    items
        .iter()
        .enumerate()
        .map(|(i, (name, desc))| {
            let marker = if i == selected { " › " } else { "   " };
            let name_span = if i == selected {
                Span::styled(
                    format!("{name:<name_w$}"),
                    Style::default().add_modifier(Modifier::BOLD),
                )
            } else {
                Span::raw(format!("{name:<name_w$}"))
            };
            Line::from(vec![
                Span::styled(marker.to_string(), Style::default().fg(theme::MUTED)),
                name_span,
                Span::styled(format!("  {desc}"), Style::default().fg(theme::MUTED)),
            ])
        })
        .collect()
}

/// Renders the 2-row input form: a DIM full-width rule, then `› {input}`
/// with a right-aligned suffix ([`InputSuffix`]). Replaces the old bordered
/// "task" box entirely — `DESIGN.md`: input is a single borderless `›`
/// line, no boxes, dim `─` rules only. Positions the terminal cursor at the
/// end of the typed text, except while a picker or permission prompt has
/// focus.
fn draw_input(f: &mut ratatui::Frame, area: ratatui::layout::Rect, state: &AppState) {
    if area.height == 0 {
        return;
    }
    let rule_area = ratatui::layout::Rect { height: 1, ..area };
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            "─".repeat(area.width as usize),
            Style::default().fg(theme::DIM),
        ))),
        rule_area,
    );

    if area.height < 2 {
        return;
    }
    let line_area = ratatui::layout::Rect {
        y: area.y + 1,
        height: 1,
        ..area
    };

    let suffix = input_suffix(state.knowledge.as_ref());
    let suffix_text = suffix.plain_text();
    let prefix_width = 3 + state.input.chars().count(); // " › " + input
    let pad = (line_area.width as usize).saturating_sub(prefix_width + suffix_text.chars().count());

    let mut spans = vec![
        Span::styled(" › ", Style::default().fg(theme::MUTED)),
        Span::raw(state.input.clone()),
        Span::raw(" ".repeat(pad)),
    ];
    spans.extend(suffix.spans());
    f.render_widget(Paragraph::new(Line::from(spans)), line_area);

    if !state.picker.open && state.pending.is_empty() {
        let cursor_x = (line_area.x as usize + 3 + state.input.chars().count())
            .min((line_area.x + line_area.width).saturating_sub(1) as usize)
            as u16;
        f.set_cursor_position((cursor_x, line_area.y));
    }
}

fn task_step_label(step: TaskStep) -> &'static str {
    match step {
        TaskStep::Understand => "UNDERSTAND",
        TaskStep::Decide => "DECIDE",
        TaskStep::Change => "CHANGE",
        TaskStep::Verify => "VERIFY",
    }
}

/// Renders the Ledger view (Ctrl+L): OBJECTIVE, the 4 numbered steps
/// (`✓` done / `●` active / `○` pending — no borders, DIM rule spacing
/// only), CURRENT CHANGE, and WHY. Every row traces to a real event; no
/// invented captions.
fn draw_ledger(f: &mut ratatui::Frame, area: ratatui::layout::Rect, ledger: &LedgerState) {
    let mut lines: Vec<Line<'static>> = Vec::new();

    lines.push(Line::from(vec![
        Span::styled(
            " OBJECTIVE  ",
            Style::default()
                .fg(theme::MUTED)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(if ledger.objective.is_empty() {
            "(no task yet)".to_string()
        } else {
            ledger.objective.clone()
        }),
    ]));
    lines.push(Line::default());

    let first_undone = ledger.steps.iter().position(|(_, done)| !*done);
    for (i, (step, done)) in ledger.steps.iter().enumerate() {
        let (glyph, glyph_style) = if *done {
            ("✓", Style::default().fg(theme::OK))
        } else if first_undone == Some(i) {
            (
                "●",
                Style::default()
                    .fg(theme::MUTED)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            ("○", Style::default().fg(theme::DIM))
        };

        let caption = match step {
            TaskStep::Change => ledger.last_change.clone(),
            TaskStep::Verify if !ledger.verify_steps.is_empty() => Some(
                ledger
                    .verify_steps
                    .iter()
                    .map(|(name, status)| {
                        let mark = match status {
                            StepStatusLite::Passed => "✓",
                            StepStatusLite::Failed => "×",
                            StepStatusLite::Skipped => "–",
                        };
                        format!("{name} {mark}")
                    })
                    .collect::<Vec<_>>()
                    .join(" · "),
            ),
            _ => None,
        };

        let mut spans = vec![
            Span::styled(
                format!("   {:02}  ", i + 1),
                Style::default().fg(theme::DIM),
            ),
            Span::styled(
                format!("{:<11}", task_step_label(*step)),
                Style::default().fg(theme::MUTED),
            ),
            Span::styled(format!(" {glyph}  "), glyph_style),
        ];
        if let Some(caption) = caption {
            spans.push(Span::styled(caption, Style::default().fg(theme::DIM)));
        }
        lines.push(Line::from(spans));
    }

    lines.push(Line::default());
    lines.push(Line::from(Span::styled(
        " CURRENT CHANGE",
        Style::default()
            .fg(theme::MUTED)
            .add_modifier(Modifier::BOLD),
    )));
    lines.push(match &ledger.last_change {
        Some(change) => Line::from(Span::raw(format!("   {change}"))),
        None => Line::from(Span::styled(
            "   no edits yet",
            Style::default().fg(theme::DIM),
        )),
    });

    if !ledger.why.is_empty() {
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            " WHY",
            Style::default()
                .fg(theme::MUTED)
                .add_modifier(Modifier::BOLD),
        )));
        for (source, text) in &ledger.why {
            let (label, color) = match source {
                WhySource::Zindeks => ("Z", theme::Z),
                WhySource::Ingat => ("I", theme::I),
            };
            lines.push(Line::from(vec![
                Span::styled(
                    format!("   {label}  "),
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ),
                Span::styled(text.clone(), Style::default().fg(theme::DIM)),
            ]));
        }
    }

    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
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

    let title = match picker.kind {
        PickerKind::Model => "select model",
        PickerKind::Provider => "select provider",
    };
    let mut lines: Vec<Line> = vec![
        Line::from(Span::styled(
            format!(" {title}"),
            Style::default()
                .fg(theme::MUTED)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            "─".repeat(popup.width as usize),
            Style::default().fg(theme::DIM),
        )),
    ];

    let filtered = picker_filtered_items(&picker.items, &picker.filter);
    lines.push(Line::from(Span::styled(
        format!("filter: {}", picker.filter),
        Style::default().fg(theme::MUTED),
    )));
    lines.push(Line::from(Span::styled(
        "(type to filter; Enter on empty filter row = use typed text verbatim)",
        Style::default().fg(theme::MUTED),
    )));
    if let Some(note) = &picker.note {
        lines.push(Line::from(Span::styled(
            format!("note: {note}"),
            Style::default().fg(theme::MUTED),
        )));
    }
    if filtered.is_empty() && picker.items.is_empty() && picker.note.is_none() {
        lines.push(Line::from(Span::styled(
            "(loading…)",
            Style::default().fg(theme::DIM),
        )));
    } else if filtered.is_empty() && !picker.items.is_empty() {
        lines.push(Line::from(Span::styled(
            "(no matches)",
            Style::default().fg(theme::DIM),
        )));
    }
    for (i, item) in filtered.iter().take(12).enumerate() {
        let (marker, item_span) = if i == picker.selected {
            (
                Span::styled(" › ", Style::default().fg(theme::MUTED)),
                Span::styled(item.clone(), Style::default().add_modifier(Modifier::BOLD)),
            )
        } else {
            (Span::raw("   "), Span::raw(item.clone()))
        };
        lines.push(Line::from(vec![marker, item_span]));
    }

    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
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
        assert_eq!(s.transcript.len(), 2);
        assert_eq!(s.transcript[0].gutter, Gutter::Prose);
        assert_eq!(s.transcript[0].text, "thinking...");
        assert_eq!(
            s.transcript[1],
            TranscriptLine::new(Gutter::Tool, "read_file")
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
    fn tool_started_sets_tool_timer_and_finished_clears_it() {
        let mut s = state();
        assert!(s.tool_started.is_none());
        apply_event(
            &mut s,
            KodeEvent::ToolStarted {
                name: "read_file".into(),
            },
        );
        assert!(s.tool_started.is_some());
        apply_event(
            &mut s,
            KodeEvent::ToolFinished {
                name: "read_file".into(),
                ok: true,
            },
        );
        assert!(s.tool_started.is_none());
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
    fn knowledge_band_lines_capped_at_3_rows_with_more_suffix() {
        let ks = KnowledgeState {
            zindeks: vec!["src/foo.rs".to_string(), "src/bar.rs".to_string()],
            ingat: vec!["always prefix with rtk".to_string(), "another".to_string()],
            git: vec!["3 files changed".to_string()],
            context_tokens: 100,
            budget_tokens: 16_000,
        };
        let lines = knowledge_band_lines(&ks);
        assert_eq!(lines.len(), 3);
        assert!(line_text(&lines[0]).contains("+1 more"));
        assert!(line_text(&lines[1]).contains("+1 more"));
        assert!(!line_text(&lines[2]).contains("more"));
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
    fn parse_slash_command_provider_with_no_arg() {
        assert_eq!(
            parse_slash_command("/provider"),
            Some(SlashCommand::Provider(None))
        );
    }

    #[test]
    fn parse_slash_command_provider_with_arg() {
        assert_eq!(
            parse_slash_command("/provider codex"),
            Some(SlashCommand::Provider(Some("codex".to_string())))
        );
    }

    // -- provider auth-state annotation (pure fn) --------------------------

    #[test]
    fn provider_auth_state_codex_logged_in() {
        assert_eq!(
            provider_auth_state("codex", true, &[], false),
            " ✓ logged in"
        );
    }

    #[test]
    fn provider_auth_state_codex_not_logged_in() {
        assert_eq!(provider_auth_state("codex", false, &[], false), "");
    }

    #[test]
    fn provider_auth_state_opencode_family_matches_key() {
        let keys = vec!["opencode-go".to_string()];
        assert_eq!(
            provider_auth_state("opencode-go", false, &keys, false),
            " ✓ logged in"
        );
        assert_eq!(provider_auth_state("opencode", false, &keys, false), "");
        assert_eq!(provider_auth_state("kilo", false, &keys, false), "");
    }

    #[test]
    fn provider_auth_state_openai_uses_env_key() {
        assert_eq!(
            provider_auth_state("openai", false, &[], true),
            " ✓ logged in"
        );
        assert_eq!(provider_auth_state("openai", false, &[], false), "");
    }

    #[test]
    fn provider_auth_state_lmstudio_always_local() {
        assert_eq!(
            provider_auth_state("lmstudio", false, &[], false),
            " (local)"
        );
        assert_eq!(provider_auth_state("lmstudio", true, &[], true), " (local)");
    }

    // -- startup hint (pure fn) ---------------------------------------------

    #[test]
    fn startup_hint_fresh_with_codex_auth_shows_codex_hint() {
        assert_eq!(
            startup_hint("openai", false, false, true, false),
            Some("logged in via codex — run /provider codex to use it")
        );
    }

    #[test]
    fn startup_hint_provider_already_codex_is_none() {
        assert_eq!(startup_hint("codex", false, false, true, false), None);
    }

    #[test]
    fn startup_hint_env_key_set_is_none() {
        assert_eq!(startup_hint("openai", false, true, true, false), None);
    }

    #[test]
    fn startup_hint_nothing_is_none() {
        assert_eq!(startup_hint("openai", false, false, false, false), None);
    }

    #[test]
    fn startup_hint_opencode_key_found_when_no_codex_auth() {
        assert_eq!(
            startup_hint("openai", false, false, false, true),
            Some("opencode key found — run /provider opencode-go")
        );
    }

    #[test]
    fn startup_hint_model_already_set_is_none() {
        assert_eq!(startup_hint("openai", true, false, true, false), None);
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
        assert!(s.transcript.iter().any(|l| l.text.contains("/provider")));
        assert!(s.transcript.iter().any(|l| l.text.contains("Ctrl+L")));
    }

    #[test]
    fn handle_slash_command_provider_invalid_pushes_error_without_persisting() {
        let dir = temp_project_dir();
        let mut s = state();
        let mut cfg = KodeConfig::default();
        let (tx, _rx) = mpsc::unbounded_channel();

        handle_slash_command(
            &mut s,
            &dir,
            &mut cfg,
            &tx,
            SlashCommand::Provider(Some("bogus".to_string())),
        );

        assert_eq!(cfg.model.provider, "openai");
        assert!(
            s.transcript
                .iter()
                .any(|l| l.text.contains("invalid provider"))
        );
        assert!(!KodeConfig::config_path(&dir).exists());
    }

    #[tokio::test]
    async fn handle_slash_command_provider_valid_persists_clears_model_and_opens_picker() {
        let dir = temp_project_dir();
        let mut s = state();
        let mut cfg = KodeConfig::default();
        let (tx, _rx) = mpsc::unbounded_channel();

        handle_slash_command(
            &mut s,
            &dir,
            &mut cfg,
            &tx,
            SlashCommand::Provider(Some("codex".to_string())),
        );

        assert_eq!(s.status.provider, "codex");
        assert_eq!(cfg.model.provider, "codex");
        assert_eq!(s.status.model, "");
        assert_eq!(cfg.model.model, "");
        assert!(s.picker.open);
        assert_eq!(s.picker.kind, PickerKind::Model);

        let reloaded = KodeConfig::load(&dir).unwrap();
        assert_eq!(reloaded.model.provider, "codex");
        assert_eq!(reloaded.model.model, "");
    }

    // -- verify step events ------------------------------------------------

    #[test]
    fn verify_step_passed_pushes_verify_gutter_with_check() {
        let mut s = state();
        apply_event(
            &mut s,
            KodeEvent::VerifyStep {
                name: "cargo test".into(),
                passed: true,
                skipped: false,
                duration_ms: 1500,
            },
        );
        assert_eq!(s.transcript.len(), 1);
        assert_eq!(s.transcript[0].gutter, Gutter::Verify);
        assert_eq!(s.transcript[0].text, "cargo test · 1.5s ✓");
        assert_eq!(
            s.ledger.verify_steps,
            vec![("cargo test".to_string(), StepStatusLite::Passed)]
        );
    }

    #[test]
    fn verify_step_failed_pushes_verifyfail_gutter_with_cross() {
        let mut s = state();
        apply_event(
            &mut s,
            KodeEvent::VerifyStep {
                name: "cargo clippy".into(),
                passed: false,
                skipped: false,
                duration_ms: 300,
            },
        );
        assert_eq!(s.transcript[0].gutter, Gutter::VerifyFail);
        assert_eq!(s.transcript[0].text, "cargo clippy · 0.3s ×");
        assert_eq!(
            s.ledger.verify_steps,
            vec![("cargo clippy".to_string(), StepStatusLite::Failed)]
        );
    }

    #[test]
    fn verify_step_skipped_pushes_verifyskip_gutter_with_dash() {
        let mut s = state();
        apply_event(
            &mut s,
            KodeEvent::VerifyStep {
                name: "cargo fmt".into(),
                passed: false,
                skipped: true,
                duration_ms: 0,
            },
        );
        assert_eq!(s.transcript[0].gutter, Gutter::VerifySkip);
        assert_eq!(s.transcript[0].text, "cargo fmt · 0.0s – (skipped)");
        assert_eq!(
            s.ledger.verify_steps,
            vec![("cargo fmt".to_string(), StepStatusLite::Skipped)]
        );
    }

    // -- task progress / ledger steps ---------------------------------------

    #[test]
    fn task_progress_updates_matching_ledger_step() {
        let mut s = state();
        assert!(!s.ledger.steps[0].1); // Understand starts undone
        apply_event(
            &mut s,
            KodeEvent::TaskProgress {
                step: TaskStep::Understand,
                done: true,
            },
        );
        assert!(s.ledger.steps[0].1);
        assert!(!s.ledger.steps[2].1); // Change untouched
    }

    #[test]
    fn task_progress_change_and_verify_track_independently() {
        let mut s = state();
        apply_event(
            &mut s,
            KodeEvent::TaskProgress {
                step: TaskStep::Change,
                done: true,
            },
        );
        apply_event(
            &mut s,
            KodeEvent::TaskProgress {
                step: TaskStep::Verify,
                done: false,
            },
        );
        let change = s
            .ledger
            .steps
            .iter()
            .find(|(step, _)| *step == TaskStep::Change)
            .unwrap();
        let verify = s
            .ledger
            .steps
            .iter()
            .find(|(step, _)| *step == TaskStep::Verify)
            .unwrap();
        assert!(change.1);
        assert!(!verify.1);
    }

    // -- decide-step derivation from first ToolStarted ----------------------

    #[test]
    fn first_tool_started_marks_decide_step_done() {
        let mut s = state();
        let decide = |s: &AppState| {
            s.ledger
                .steps
                .iter()
                .find(|(step, _)| *step == TaskStep::Decide)
                .unwrap()
                .1
        };
        assert!(!decide(&s));
        apply_event(
            &mut s,
            KodeEvent::ToolStarted {
                name: "read_file".into(),
            },
        );
        assert!(decide(&s));
    }

    #[test]
    fn start_new_task_resets_decide_flag_for_next_run() {
        let mut s = state();
        apply_event(
            &mut s,
            KodeEvent::ToolStarted {
                name: "read_file".into(),
            },
        );
        s.start_new_task("second task");
        let decide = |s: &AppState| {
            s.ledger
                .steps
                .iter()
                .find(|(step, _)| *step == TaskStep::Decide)
                .unwrap()
                .1
        };
        assert!(!decide(&s));
        assert_eq!(s.ledger.objective, "second task");
    }

    // -- last_change counter accumulation ------------------------------------

    #[test]
    fn tool_finished_apply_patch_and_write_file_accumulate_last_change() {
        let mut s = state();
        apply_event(
            &mut s,
            KodeEvent::ToolFinished {
                name: "apply_patch".into(),
                ok: true,
            },
        );
        assert_eq!(s.ledger.last_change, Some("apply_patch ×1".to_string()));
        apply_event(
            &mut s,
            KodeEvent::ToolFinished {
                name: "apply_patch".into(),
                ok: true,
            },
        );
        apply_event(
            &mut s,
            KodeEvent::ToolFinished {
                name: "write_file".into(),
                ok: true,
            },
        );
        assert_eq!(
            s.ledger.last_change,
            Some("apply_patch ×2 · write_file ×1".to_string())
        );
    }

    #[test]
    fn tool_finished_other_tool_or_failure_does_not_touch_last_change() {
        let mut s = state();
        apply_event(
            &mut s,
            KodeEvent::ToolFinished {
                name: "read_file".into(),
                ok: true,
            },
        );
        assert_eq!(s.ledger.last_change, None);
        apply_event(
            &mut s,
            KodeEvent::ToolFinished {
                name: "apply_patch".into(),
                ok: false,
            },
        );
        assert_eq!(s.ledger.last_change, None);
    }

    // -- ledger objective / why derivation -----------------------------------

    #[test]
    fn ledger_objective_truncates_first_line_to_70_chars() {
        let long = "a".repeat(100);
        let task = format!("{long}\nsecond line");
        let objective = ledger_objective(&task);
        assert_eq!(objective.chars().count(), 71); // 70 + ellipsis
        assert!(objective.ends_with('…'));
    }

    #[test]
    fn knowledge_event_sets_ledger_why_from_first_zindeks_and_ingat() {
        let mut s = state();
        apply_event(
            &mut s,
            KodeEvent::Knowledge {
                zindeks: vec!["src/foo.rs".to_string(), "src/bar.rs".to_string()],
                ingat: vec!["always prefix with rtk".to_string()],
                git: vec![],
                context_tokens: 100,
                budget_tokens: 16_000,
            },
        );
        assert_eq!(
            s.ledger.why,
            vec![
                (WhySource::Zindeks, "src/foo.rs".to_string()),
                (WhySource::Ingat, "always prefix with rtk".to_string()),
            ]
        );
    }

    // -- knowledge aperture ---------------------------------------------------

    #[test]
    fn knowledge_event_with_data_opens_aperture() {
        let mut s = state();
        apply_event(
            &mut s,
            KodeEvent::Knowledge {
                zindeks: vec!["src/foo.rs".to_string()],
                ingat: vec![],
                git: vec![],
                context_tokens: 100,
                budget_tokens: 16_000,
            },
        );
        assert!(s.aperture.is_some());
        assert!(!s.aperture.as_ref().unwrap().trigger_seen);
    }

    #[test]
    fn knowledge_event_with_no_data_does_not_open_aperture() {
        let mut s = state();
        apply_event(
            &mut s,
            KodeEvent::Knowledge {
                zindeks: vec![],
                ingat: vec![],
                git: vec![],
                context_tokens: 0,
                budget_tokens: 16_000,
            },
        );
        assert!(s.aperture.is_none());
    }

    #[test]
    fn knowledge_event_git_only_does_not_open_aperture() {
        let mut s = state();
        apply_event(
            &mut s,
            KodeEvent::Knowledge {
                zindeks: vec![],
                ingat: vec![],
                git: vec!["3 files changed".to_string()],
                context_tokens: 0,
                budget_tokens: 16_000,
            },
        );
        assert!(s.aperture.is_none());
        // The Band still gets the git fact — only the Aperture is gated.
        assert_eq!(
            s.knowledge.as_ref().unwrap().git,
            vec!["3 files changed".to_string()]
        );
    }

    #[test]
    fn model_token_after_knowledge_marks_aperture_trigger_seen() {
        let mut s = state();
        apply_event(
            &mut s,
            KodeEvent::Knowledge {
                zindeks: vec!["src/foo.rs".to_string()],
                ingat: vec![],
                git: vec![],
                context_tokens: 100,
                budget_tokens: 16_000,
            },
        );
        apply_event(&mut s, KodeEvent::ModelToken { text: "hi".into() });
        assert!(s.aperture.as_ref().unwrap().trigger_seen);
    }

    #[test]
    fn tool_started_after_knowledge_marks_aperture_trigger_seen() {
        let mut s = state();
        apply_event(
            &mut s,
            KodeEvent::Knowledge {
                zindeks: vec!["src/foo.rs".to_string()],
                ingat: vec![],
                git: vec![],
                context_tokens: 100,
                budget_tokens: 16_000,
            },
        );
        apply_event(
            &mut s,
            KodeEvent::ToolStarted {
                name: "read_file".into(),
            },
        );
        assert!(s.aperture.as_ref().unwrap().trigger_seen);
    }

    #[test]
    fn start_new_task_clears_leftover_aperture() {
        let mut s = state();
        apply_event(
            &mut s,
            KodeEvent::Knowledge {
                zindeks: vec!["src/foo.rs".to_string()],
                ingat: vec![],
                git: vec![],
                context_tokens: 100,
                budget_tokens: 16_000,
            },
        );
        assert!(s.aperture.is_some());
        s.start_new_task("next task");
        assert!(s.aperture.is_none());
    }

    // -- aperture_should_collapse (pure decision fn) -------------------------

    #[test]
    fn aperture_never_collapses_before_trigger_seen() {
        let received = Instant::now();
        let now = received + Duration::from_secs(5);
        assert!(!aperture_should_collapse(received, now, false));
    }

    #[test]
    fn aperture_does_not_collapse_before_900ms_even_with_trigger() {
        let received = Instant::now();
        let now = received + Duration::from_millis(500);
        assert!(!aperture_should_collapse(received, now, true));
    }

    #[test]
    fn aperture_collapses_at_or_after_900ms_with_trigger() {
        let received = Instant::now();
        let now = received + Duration::from_millis(900);
        assert!(aperture_should_collapse(received, now, true));
        let later = received + Duration::from_secs(3);
        assert!(aperture_should_collapse(received, later, true));
    }

    // -- gutter mapping (extended) --------------------------------------------

    #[test]
    fn gutter_prefix_verify_variants_share_glyph_distinct_colors() {
        assert_eq!(gutter_prefix(&Gutter::Verify), ("V ", theme::OK));
        assert_eq!(gutter_prefix(&Gutter::VerifyFail), ("V ", theme::ERR));
        assert_eq!(gutter_prefix(&Gutter::VerifySkip), ("V ", theme::DIM));
    }

    #[test]
    fn md_span_style_italic_renders_plain() {
        // DESIGN.md: italic is reserved for verbatim quoted ingat memory —
        // markdown emphasis never gets the ITALIC modifier.
        let style = md_span_style(&markdown::MdStyle::Italic);
        assert!(!style.add_modifier.contains(Modifier::ITALIC));
    }

    // -- idle empty-state visibility (pure fn) -------------------------------

    #[test]
    fn show_empty_state_true_on_fresh_session() {
        assert!(show_empty_state(&[], false));
    }

    #[test]
    fn show_empty_state_false_while_running() {
        assert!(!show_empty_state(&[], true));
    }

    #[test]
    fn show_empty_state_true_with_only_startup_note_hints() {
        let transcript = vec![TranscriptLine::new(Gutter::Note, "pick a model first")];
        assert!(show_empty_state(&transcript, false));
    }

    #[test]
    fn show_empty_state_false_once_real_content_present() {
        let transcript = vec![
            TranscriptLine::new(Gutter::Note, "pick a model first"),
            TranscriptLine::new(Gutter::User, "fix the bug"),
        ];
        assert!(!show_empty_state(&transcript, false));
    }

    // -- engine status (pure fn) ---------------------------------------------

    #[test]
    fn engine_status_disabled_overrides_everything() {
        assert_eq!(engine_status(false, true), EngineStatus::Disabled);
        assert_eq!(engine_status(false, false), EngineStatus::Disabled);
    }

    #[test]
    fn engine_status_ready_when_enabled_and_source_seen() {
        assert_eq!(engine_status(true, true), EngineStatus::Ready);
    }

    #[test]
    fn engine_status_available_after_first_task_when_enabled_not_seen() {
        assert_eq!(
            engine_status(true, false),
            EngineStatus::AvailableAfterFirstTask
        );
    }

    // -- input suffix (pure fn) -----------------------------------------------

    #[test]
    fn input_suffix_help_when_no_knowledge_yet() {
        assert_eq!(input_suffix(None), InputSuffix::Help);
        assert_eq!(input_suffix(None).plain_text(), "/help");
    }

    #[test]
    fn input_suffix_counts_when_knowledge_present() {
        let ks = KnowledgeState {
            zindeks: vec!["a".to_string(), "b".to_string()],
            ingat: vec!["c".to_string()],
            git: vec![],
            context_tokens: 100,
            budget_tokens: 16_000,
        };
        assert_eq!(
            input_suffix(Some(&ks)),
            InputSuffix::Counts { z: 2, i: 1, g: 0 }
        );
        assert_eq!(input_suffix(Some(&ks)).plain_text(), "ctx Z:2 I:1 G:0");
    }

    // -- breadcrumb model nudge -------------------------------------------

    fn line_text(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn breadcrumb_line_nudges_when_model_unset() {
        let s = state_no_model();
        assert!(line_text(&breadcrumb_line(&s)).contains("— /model"));
    }

    #[test]
    fn breadcrumb_line_omits_nudge_when_model_set() {
        let s = state();
        assert!(!line_text(&breadcrumb_line(&s)).contains("— /model"));
    }

    fn state_no_model() -> AppState {
        AppState::new("openai".to_string(), String::new(), String::new())
    }

    // -- auto mode (Shift+Tab) ------------------------------------------------

    #[test]
    fn backtab_toggles_auto_mode_and_shared_flag() {
        let mut s = state();
        assert!(!s.auto_mode);
        assert!(!s.auto_flag.load(Ordering::Relaxed));

        handle_key(&mut s, KeyCode::BackTab, KeyModifiers::NONE, &None);
        assert!(s.auto_mode);
        assert!(s.auto_flag.load(Ordering::Relaxed));
        assert!(s.transcript.iter().any(|l| l.text.contains("auto mode on")));

        handle_key(&mut s, KeyCode::BackTab, KeyModifiers::NONE, &None);
        assert!(!s.auto_mode);
        assert!(!s.auto_flag.load(Ordering::Relaxed));
        assert!(s.transcript.iter().any(|l| l.text == "auto mode off"));
    }

    #[test]
    fn breadcrumb_line_shows_auto_badge_only_when_on() {
        let mut s = state();
        assert!(!line_text(&breadcrumb_line(&s)).contains("auto"));
        s.auto_mode = true;
        assert!(line_text(&breadcrumb_line(&s)).contains("· auto"));
    }

    #[tokio::test]
    async fn tui_permission_confirm_bypasses_prompt_when_auto_flag_set() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let auto = Arc::new(AtomicBool::new(true));
        let handler = TuiPermission::new(tx, auto);
        assert!(handler.confirm("run rm -rf").await);
        assert!(rx.try_recv().is_err()); // no prompt was ever queued
    }

    #[tokio::test]
    async fn tui_permission_confirm_queues_prompt_when_auto_flag_unset() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let auto = Arc::new(AtomicBool::new(false));
        let handler = TuiPermission::new(tx, auto);
        let confirm = tokio::spawn(async move { handler.confirm("run rm -rf").await });
        let (summary, responder) = rx.recv().await.expect("prompt queued");
        assert_eq!(summary, "run rm -rf");
        responder.send(true).unwrap();
        assert!(confirm.await.unwrap());
    }

    // -- clipboard copy (Ctrl+Y / /copy) --------------------------------------

    #[test]
    fn perform_copy_with_empty_last_response_notes_nothing_to_copy() {
        let mut s = state();
        perform_copy(&mut s);
        assert_eq!(
            s.transcript,
            vec![TranscriptLine::new(Gutter::Note, "nothing to copy yet")]
        );
    }

    #[test]
    fn ctrl_y_triggers_copy_path() {
        let mut s = state();
        handle_key(&mut s, KeyCode::Char('y'), KeyModifiers::CONTROL, &None);
        assert_eq!(s.transcript.len(), 1);
        assert_eq!(s.transcript[0].gutter, Gutter::Note);
    }

    #[test]
    fn parse_slash_command_copy() {
        assert_eq!(parse_slash_command("/copy"), Some(SlashCommand::Copy));
    }

    #[test]
    fn handle_slash_command_copy_dispatches_to_perform_copy() {
        let dir = temp_project_dir();
        let mut s = state();
        let mut cfg = KodeConfig::default();
        let (tx, _rx) = mpsc::unbounded_channel();

        handle_slash_command(&mut s, &dir, &mut cfg, &tx, SlashCommand::Copy);
        assert!(s.transcript.iter().any(|l| l.text == "nothing to copy yet"));
    }

    #[cfg(windows)]
    #[test]
    fn copy_to_clipboard_via_clip_exe_returns_char_count() {
        let result = copy_to_clipboard("hello kode");
        assert_eq!(result, Ok(10));
    }

    #[test]
    fn handle_slash_command_help_mentions_new_shortcuts() {
        let dir = temp_project_dir();
        let mut s = state();
        let mut cfg = KodeConfig::default();
        let (tx, _rx) = mpsc::unbounded_channel();

        handle_slash_command(&mut s, &dir, &mut cfg, &tx, SlashCommand::Help);
        assert!(s.transcript.iter().any(|l| l.text.contains("shift+tab")));
        assert!(s.transcript.iter().any(|l| l.text.contains("ctrl+y")));
        assert!(s.transcript.iter().any(|l| l.text.contains("/copy")));
    }

    // -- last_response tracking -----------------------------------------------

    #[test]
    fn task_finished_swaps_flushed_prose_into_last_response() {
        let mut s = state();
        apply_event(
            &mut s,
            KodeEvent::ModelToken {
                text: "done here".into(),
            },
        );
        apply_event(
            &mut s,
            KodeEvent::TaskFinished {
                iterations: 1,
                tool_calls: 0,
                input_tokens: 10,
                output_tokens: 5,
            },
        );
        assert_eq!(s.last_response, "done here");
    }

    #[test]
    fn agent_error_swaps_flushed_prose_into_last_response() {
        let mut s = state();
        apply_event(
            &mut s,
            KodeEvent::ModelToken {
                text: "partial answer".into(),
            },
        );
        apply_event(
            &mut s,
            KodeEvent::AgentError {
                message: "boom".into(),
            },
        );
        assert_eq!(s.last_response, "partial answer");
    }

    #[test]
    fn start_new_task_does_not_clear_prior_last_response() {
        let mut s = state();
        s.last_response = "prior answer".to_string();
        s.start_new_task("next task");
        assert_eq!(s.last_response, "prior answer");
    }

    // -- session persistence (record_completed_turn) ---------------------------

    #[test]
    fn task_finished_appends_turn_to_in_memory_history() {
        let dir = temp_project_dir();
        let mut s = state();

        s.start_new_task("first task");
        apply_event(
            &mut s,
            KodeEvent::ModelToken {
                text: "first answer".into(),
            },
        );
        apply_event(
            &mut s,
            KodeEvent::TaskFinished {
                iterations: 1,
                tool_calls: 2,
                input_tokens: 10,
                output_tokens: 5,
            },
        );
        record_completed_turn(&mut s, &dir, "codex", "gpt-test", 2);

        assert_eq!(s.history.len(), 1);
        assert_eq!(s.history[0].task, "first task");
        assert_eq!(s.history[0].response, "first answer");
        assert_eq!(s.history[0].tool_calls, 2);
        assert!(s.pending_task.is_none());
        let id = s.session_id.clone().expect("session id set");
        let (turns, corrupt) = crate::session::load(&dir, &id).unwrap();
        assert_eq!(corrupt, 0);
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].task, "first task");

        // Second task appends to the same session.
        s.start_new_task("second task");
        apply_event(
            &mut s,
            KodeEvent::ModelToken {
                text: "second answer".into(),
            },
        );
        apply_event(
            &mut s,
            KodeEvent::TaskFinished {
                iterations: 1,
                tool_calls: 0,
                input_tokens: 1,
                output_tokens: 1,
            },
        );
        record_completed_turn(&mut s, &dir, "codex", "gpt-test", 0);

        assert_eq!(s.history.len(), 2);
        assert_eq!(s.session_id.as_deref(), Some(id.as_str()));
        let (turns, _) = crate::session::load(&dir, &id).unwrap();
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[1].task, "second task");
    }

    #[test]
    fn agent_error_discards_pending_task() {
        let dir = temp_project_dir();
        let mut s = state();

        s.start_new_task("will fail");
        apply_event(
            &mut s,
            KodeEvent::AgentError {
                message: "boom".into(),
            },
        );
        s.pending_task = None; // mirrors the run loop's AgentError arm

        assert!(s.history.is_empty());
        assert!(s.session_id.is_none());
        assert!(!dir.join(".kode").join("sessions").exists());
    }

    // -- markdown flush integration --------------------------------------------

    #[test]
    fn flushed_heading_line_gets_markdown_spans_and_leading_spacer() {
        let mut s = state();
        apply_event(
            &mut s,
            KodeEvent::ModelToken {
                text: "## Plan".into(),
            },
        );
        apply_event(
            &mut s,
            KodeEvent::ToolStarted {
                name: "read_file".into(),
            },
        );
        // spacer blank line, then the heading, then the tool line.
        assert_eq!(s.transcript.len(), 3);
        assert_eq!(s.transcript[0].gutter, Gutter::None);
        assert_eq!(s.transcript[1].gutter, Gutter::Prose);
        assert_eq!(s.transcript[1].md_kind, Some(markdown::MdKind::Heading));
        assert_eq!(
            s.transcript[1].spans,
            Some(vec![("Plan".to_string(), markdown::MdStyle::Bold)])
        );
    }

    #[test]
    fn flushed_bold_line_splits_into_styled_spans() {
        let mut s = state();
        apply_event(
            &mut s,
            KodeEvent::ModelToken {
                text: "do **this**".into(),
            },
        );
        apply_event(&mut s, KodeEvent::AgentFinished);
        let prose = s
            .transcript
            .iter()
            .find(|l| l.gutter == Gutter::Prose)
            .expect("prose line");
        assert_eq!(prose.md_kind, Some(markdown::MdKind::Plain));
        assert_eq!(
            prose.spans,
            Some(vec![
                ("do ".to_string(), markdown::MdStyle::Plain),
                ("this".to_string(), markdown::MdStyle::Bold),
            ])
        );
    }

    // -- slash-command hint menu -----------------------------------------------

    #[test]
    fn slash_hint_items_bare_slash_lists_all() {
        assert_eq!(slash_hint_items("/").len(), SLASH_COMMANDS.len());
    }

    #[test]
    fn slash_hint_items_prefix_narrows() {
        let items = slash_hint_items("/mo");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].0, "/model");
    }

    #[test]
    fn slash_hint_items_non_slash_is_empty() {
        assert!(slash_hint_items("hello").is_empty());
    }

    #[test]
    fn slash_hint_items_hides_once_args_typed() {
        assert!(slash_hint_items("/model g").is_empty());
    }

    #[test]
    fn handle_key_down_moves_hint_selection_not_scroll() {
        let mut s = state();
        s.input = "/".to_string();
        handle_key(&mut s, KeyCode::Down, KeyModifiers::NONE, &None);
        assert_eq!(s.slash_selected, 1);
        assert_eq!(s.scroll, 0);
    }

    #[test]
    fn handle_key_up_down_clamp_hint_selection() {
        let mut s = state();
        s.input = "/".to_string();
        for _ in 0..SLASH_COMMANDS.len() + 2 {
            handle_key(&mut s, KeyCode::Down, KeyModifiers::NONE, &None);
        }
        assert_eq!(s.slash_selected, SLASH_COMMANDS.len() - 1);
        for _ in 0..SLASH_COMMANDS.len() + 2 {
            handle_key(&mut s, KeyCode::Up, KeyModifiers::NONE, &None);
        }
        assert_eq!(s.slash_selected, 0);
    }

    #[test]
    fn handle_key_tab_completes_highlighted_command() {
        let mut s = state();
        s.input = "/".to_string();
        handle_key(&mut s, KeyCode::Down, KeyModifiers::NONE, &None);
        handle_key(&mut s, KeyCode::Tab, KeyModifiers::NONE, &None);
        assert_eq!(s.input, "/effort ");
        assert_eq!(s.slash_selected, 0);
    }

    #[test]
    fn handle_key_typing_resets_hint_selection() {
        let mut s = state();
        s.input = "/".to_string();
        s.slash_selected = 2;
        handle_key(&mut s, KeyCode::Char('m'), KeyModifiers::NONE, &None);
        assert_eq!(s.slash_selected, 0);
        assert_eq!(s.input, "/m");
    }

    #[test]
    fn handle_key_esc_clears_input_when_hint_visible() {
        let mut s = state();
        s.input = "/mo".to_string();
        handle_key(&mut s, KeyCode::Esc, KeyModifiers::NONE, &None);
        assert!(s.input.is_empty());
        assert!(!s.running);
    }

    #[test]
    fn slash_hint_lines_marks_selected_row() {
        let items: Vec<(&'static str, &'static str)> = vec![("/model", "a"), ("/effort", "b")];
        let lines = slash_hint_lines(&items, 1);
        assert!(!lines[0].spans[0].content.contains('›'));
        assert!(lines[1].spans[0].content.contains('›'));
    }
}
