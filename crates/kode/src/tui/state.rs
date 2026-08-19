use std::collections::VecDeque;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

use kode_context::git::{NumstatRow, RepoState};
use kode_core::event::TaskStep;
use tokio::sync::oneshot;

use super::markdown;

/// The agent run's current phase, shown in the breadcrumb/spinner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunState {
    Idle,
    Thinking,
    Tool,
    Verify,
}

impl RunState {
    pub(crate) fn label(self) -> &'static str {
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
    /// A knowledge-derived note attributed to zindeks.
    Zindeks,
    /// A knowledge-derived note attributed to ingat.
    Ingat,
    /// A knowledge-derived note attributed to git.
    Git,
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
    /// Names of the tool calls stacked behind this line when it's a
    /// collapsible tool-group header (see `KodeEvent::ToolStarted`
    /// grouping in `events.rs`). Empty for every non-header line.
    pub tool_children: Vec<String>,
    /// Whether a tool-group header (`tool_children` non-empty) is expanded
    /// to show its stacked children — toggled by left-clicking the header
    /// (see `run::handle_mouse`). Ignored when `tool_children` is empty.
    pub expanded: bool,
}

impl TranscriptLine {
    pub fn new(gutter: Gutter, text: impl Into<String>) -> Self {
        Self {
            gutter,
            text: text.into(),
            md_kind: None,
            spans: None,
            tool_children: Vec::new(),
            expanded: false,
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
            tool_children: Vec::new(),
            expanded: false,
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
    /// `render_tick` (see `AppState::render_tick`) at which the current
    /// `zindeks.first()` fact first appeared — carried over unchanged
    /// across `Knowledge` events while that fact stays the same, reset
    /// when it changes. Drives the knowledge-band dim→normal fade.
    pub zindeks_since_tick: Option<u64>,
    /// Same as `zindeks_since_tick`, for `ingat.first()`.
    pub ingat_since_tick: Option<u64>,
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
/// invented captions. `steps` is the fixed 4-step lifecycle (Understand,
/// Decide, Change, Verify), with a `Plan` step prepended when the task was
/// submitted under plan mode (see [`LedgerState::new`]).
#[derive(Debug, Clone)]
pub struct LedgerState {
    pub objective: String,
    pub steps: Vec<(TaskStep, bool)>,
    pub verify_steps: Vec<(String, StepStatusLite)>,
    /// Real per-file `git diff --numstat` rows for the CURRENT CHANGE
    /// section — refreshed by the same lazy git poll that drives the
    /// breadcrumb's dirty indicator (see `spawn_git_poll`/`apply_repo_state`),
    /// not by counting tool calls.
    pub numstat: Vec<NumstatRow>,
    pub why: Vec<(WhySource, String)>,
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
            numstat: Vec::new(),
            why: Vec::new(),
        }
    }
}

impl LedgerState {
    /// `plan_mode` prepends `(TaskStep::Plan, false)` ahead of the fixed
    /// 4-step lifecycle — the Ledger renders it as step 01 and it's marked
    /// done once the plan is approved (see `pipeline::run_plan_phase`).
    fn new(objective: String, plan_mode: bool) -> Self {
        let mut steps = Vec::with_capacity(5);
        if plan_mode {
            steps.push((TaskStep::Plan, false));
        }
        steps.extend(Self::default().steps);
        Self {
            objective,
            steps,
            ..Default::default()
        }
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
    Session,
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
    /// Working-tree dirty flag from the lazy git poll (TUI start + after
    /// each task completes — see `spawn_git_poll`). Drives the breadcrumb's
    /// dim `*` suffix on the branch segment.
    pub dirty: bool,
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
    pub(crate) decide_marked_this_run: bool,
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
    /// Plan mode (`/plan`): when on, a submitted task first produces a
    /// numbered plan (a tools-disabled model turn) and asks for approval
    /// before the real task runs. Session-only — never persisted to config.
    /// Read once per submission in `submit_task`, unlike `auto_mode` which
    /// also has a shared atomic for mid-run reads from the permission
    /// handler — plan mode is only ever consulted at submission time.
    pub plan_mode: bool,
    /// The most recently *completed* agent message's full text — what
    /// Ctrl+Y/`/copy` copy to the clipboard. Empty until the first message
    /// finishes.
    pub last_response: String,
    /// Accumulates flushed prose chunks for the run currently in flight;
    /// swapped into `last_response` on `TaskFinished`/`AgentError`.
    pub(crate) response_buf: String,
    /// Per-message ``` fence state for markdown rendering of flushed Prose
    /// lines; reset on every new task submission.
    pub(crate) md_in_code_block: bool,
    /// Highlighted row in the slash-command hint menu.
    pub slash_selected: usize,
    /// Completed turns of the active session — sent as model history.
    pub history: Vec<crate::session::Turn>,
    /// Active session file id; created lazily on first completed task.
    pub session_id: Option<String>,
    /// Task text of the in-flight run; consumed when TaskFinished arrives.
    pub pending_task: Option<String>,
    /// `[ui].reduced_motion` from config. When true: spinner glyph is
    /// static, knowledge-band evidence rows skip the dim→normal fade, and
    /// the Ledger active marker doesn't pulse. Streaming coalescing stays
    /// active regardless — it's buffering, not motion.
    pub reduced_motion: bool,
    /// Monotonic counter bumped once per ~100ms UI tick (see `run`'s
    /// `aperture_tick`), used only to timestamp when a knowledge-band
    /// evidence row first appeared, for the dim→normal fade.
    pub render_tick: u64,
    /// Buffered `ModelToken` deltas not yet flushed into `current_stream`
    /// (and therefore not yet visible). Flushed on a word/whitespace
    /// boundary or a 120ms timer — see `should_flush_stream_buffer`.
    pub(crate) stream_pending: String,
    /// When the current `stream_pending` buffering window started; `None`
    /// while the buffer is empty / just flushed.
    pub(crate) stream_last_flush: Option<Instant>,
    /// User-toggled select mode (Ctrl+T): while true, mouse capture is
    /// released to the terminal so the user can drag-select/copy text
    /// natively; wheel scroll and click-to-expand stop working until it's
    /// toggled back off. Defaults off (mouse capture on, as today).
    pub select_mode: bool,
    /// Hit-test geometry for the last-rendered transcript, rebuilt every
    /// frame in `draw()`. `None` while the Ledger view is showing (no
    /// transcript to click). Drives left-click-to-expand on tool-group
    /// headers.
    pub transcript_hit: Option<TranscriptHit>,
}

/// Per-frame click hit-test geometry for the transcript area, rebuilt by
/// `draw()` every render. `rows` walks the expanded (post-collapse) line
/// list in render order: each entry is `(wrapped_row_count, transcript_idx)`
/// where `transcript_idx` is `Some(i)` when that logical line is a
/// clickable tool-group header (`state.transcript[i]`), `None` for
/// everything else (prose, plain tool lines, expanded children, the stream
/// line, the spinner label).
#[derive(Debug, Clone, Default)]
pub struct TranscriptHit {
    pub area: ratatui::layout::Rect,
    pub scroll: u16,
    pub rows: Vec<(u16, Option<usize>)>,
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
            dirty: false,
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
            plan_mode: false,
            last_response: String::new(),
            response_buf: String::new(),
            md_in_code_block: false,
            slash_selected: 0,
            history: Vec::new(),
            session_id: None,
            pending_task: None,
            reduced_motion: false,
            render_tick: 0,
            stream_pending: String::new(),
            stream_last_flush: None,
            select_mode: false,
            transcript_hit: None,
        }
    }

    pub fn push_permission(&mut self, req: PermReq) {
        self.pending.push_back(req);
    }

    pub fn pop_permission(&mut self) -> Option<PermReq> {
        self.pending.pop_front()
    }

    /// Resets per-run state for a freshly submitted task: the Ledger
    /// (objective + steps, with a leading Plan step when `plan_mode` is on),
    /// the Decide-derivation flag, and any leftover Aperture from a prior
    /// run. Pure — the caller still owns emitting the actual task to the
    /// pipeline.
    pub fn start_new_task(&mut self, task: &str, plan_mode: bool) {
        self.ledger = LedgerState::new(ledger_objective(task), plan_mode);
        self.decide_marked_this_run = false;
        self.aperture = None;
        self.tool_started = None;
        self.response_buf.clear();
        self.md_in_code_block = false;
        self.stream_pending.clear();
        self.stream_last_flush = None;
        self.pending_task = Some(task.to_string());
    }
}

/// Persists the in-flight task (if any) as a completed `session::Turn`: both
/// to disk (creating the session lazily on first write) and into
/// `state.history` for the next task's model replay. No-op when
/// `pending_task` is `None` (nothing was in flight — e.g. a stray event).
/// Store I/O failures are surfaced as transcript Notes, never fatal.
pub(crate) fn record_completed_turn(
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

/// Loads session `id` into the app: history armed for the model, transcript
/// replayed for the human. Returns false (with a transcript Note) on
/// failure or an empty session.
pub(crate) fn restore_session(state: &mut AppState, cwd: &Path, id: &str) -> bool {
    match crate::session::load(cwd, id) {
        Ok((turns, corrupt)) => {
            if turns.is_empty() {
                state.transcript.push(TranscriptLine::new(
                    Gutter::Note,
                    format!("session {id} has no turns"),
                ));
                return false;
            }
            state.transcript.clear();
            state.history.clear();
            state.transcript.push(TranscriptLine::new(
                Gutter::Note,
                format!("— resumed {id} · {} turns —", turns.len()),
            ));
            if corrupt > 0 {
                state.transcript.push(TranscriptLine::new(
                    Gutter::Note,
                    format!("session {id}: skipped {corrupt} corrupt lines"),
                ));
            }
            for t in &turns {
                state
                    .transcript
                    .push(TranscriptLine::new(Gutter::User, t.task.clone()));
                let mut in_code_block = false;
                for line in t.response.lines() {
                    let rendered = markdown::render_line(line, &mut in_code_block);
                    state.transcript.push(TranscriptLine::markdown(
                        Gutter::Prose,
                        line,
                        rendered.kind,
                        rendered.spans,
                    ));
                }
            }
            state.last_response = turns.last().map(|t| t.response.clone()).unwrap_or_default();
            state.session_id = Some(id.to_string());
            state.history = turns;
            true
        }
        Err(e) => {
            state.transcript.push(TranscriptLine::new(
                Gutter::Note,
                format!("could not load session {id}: {e}"),
            ));
            false
        }
    }
}

/// First line of `task`, truncated to 70 chars (char-safe) — the Ledger
/// view's OBJECTIVE text.
pub(crate) fn ledger_objective(task: &str) -> String {
    let first_line = task.lines().next().unwrap_or("");
    truncate_chars(first_line, 70)
}

/// Applies a lazily-polled `RepoState` (see `spawn_git_poll`) to `state`:
/// the breadcrumb's dirty flag and the Ledger's CURRENT CHANGE numstat
/// rows. Pure — no I/O, called from the `git_rx` arm of the event loop.
pub(crate) fn apply_repo_state(state: &mut AppState, repo: RepoState) {
    state.dirty = repo.dirty;
    state.ledger.numstat = repo.numstat;
}

/// The Ledger's Change-step inline caption: `"{n} files changed"` when the
/// git poll found diff rows, else `None` (no invented caption before the
/// first poll resolves or on a clean tree).
pub(crate) fn numstat_caption(numstat: &[NumstatRow]) -> Option<String> {
    if numstat.is_empty() {
        None
    } else {
        Some(format!("{} files changed", numstat.len()))
    }
}

/// First whitespace-separated token of a `/resume` picker row (the session
/// id) — rows are formatted `"<id>  <first-task>  · <N> turns"`.
pub(crate) fn session_id_from_row(row: &str) -> String {
    row.split_whitespace().next().unwrap_or("").to_string()
}

/// Truncates `s` to at most `max` chars, appending `…` when truncated.
/// Char-safe (splits on `char_indices`, never mid-codepoint).
pub(crate) fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut truncated: String = s.chars().take(max).collect();
        truncated.push('…');
        truncated
    }
}
