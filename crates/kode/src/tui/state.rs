use std::collections::VecDeque;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use kode_context::git::{NumstatRow, RepoState};
use kode_core::event::TaskStep;
use kode_core::{ImageAttachment, UserInput};
use tokio::sync::oneshot;

use super::markdown;

pub(crate) const INTERRUPT_CONFIRM_WINDOW: Duration = Duration::from_secs(2);
pub(crate) const PASTE_ATTACHMENT_MIN_LINES: usize = 8;
pub(crate) const PASTE_ATTACHMENT_MIN_CHARS: usize = 1000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PastedAttachment {
    pub name: String,
    pub content: String,
    pub line_count: usize,
    pub char_count: usize,
}

impl PastedAttachment {
    pub(crate) fn summary(&self) -> String {
        format!(
            "{} · {} lines · {} chars",
            self.name,
            self.line_count,
            compact_count(self.char_count)
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachmentKind {
    Text,
    Image,
}

/// The agent run's current phase, shown in the breadcrumb/spinner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunState {
    Idle,
    Thinking,
    Tool,
    Verify,
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
    /// Aggregate duration of the completed tool call(s) represented by this
    /// receipt. Kept separate from `text` so grouping remains stable.
    pub tool_duration_ms: Option<u128>,
    /// `None` while the represented tool group is active, otherwise the
    /// aggregate outcome used by the transcript receipt suffix.
    pub tool_ok: Option<bool>,
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
            tool_duration_ms: None,
            tool_ok: None,
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
            tool_duration_ms: None,
            tool_ok: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionReceipt {
    pub iterations: u32,
    pub tool_calls: u32,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub elapsed_ms: u128,
    pub verify_steps: Vec<(String, StepStatusLite)>,
    pub numstat: Vec<NumstatRow>,
}

impl CompletionReceipt {
    pub(crate) fn verification_label(&self) -> &'static str {
        if self
            .verify_steps
            .iter()
            .any(|(_, status)| *status == StepStatusLite::Failed)
        {
            "DONE · FAILED VERIFICATION"
        } else if self.verify_steps.is_empty()
            || self
                .verify_steps
                .iter()
                .all(|(_, status)| *status == StepStatusLite::Skipped)
        {
            "DONE · UNVERIFIED"
        } else {
            "DONE · VERIFIED"
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComposerMode {
    Ask,
    Steer,
    Queue,
    Decision,
    Recover,
    FollowUp,
}

impl ComposerMode {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Ask => "ASK KODE",
            Self::Steer => "STEER ACTIVE RUN",
            Self::Queue => "QUEUE NEXT TASK",
            Self::Decision => "DECISION REQUIRED",
            Self::Recover => "RECOVER OR ASK AGAIN",
            Self::FollowUp => "ASK A FOLLOW-UP",
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
    /// Whether the active pipeline currently has an agent receiver ready for
    /// steering. False during verification/finalization; input submitted in
    /// that window is queued as the next turn by the TUI loop.
    pub steering_active: bool,
    pub pending: VecDeque<PermReq>,
    pub scroll: u16,
    pub follow: bool,
    pub input: String,
    /// Large/multiline bracketed pastes kept outside the visible text input.
    /// They are materialized into the submitted task only when Enter is
    /// pressed, so the composer stays compact.
    pub pasted_attachments: Vec<PastedAttachment>,
    /// Image files attached to the next user turn. Binary data stays outside
    /// the visible composer and is sent as structured provider content.
    pub image_attachments: Vec<ImageAttachment>,
    /// Cross-type insertion order so rendering and Backspace both operate
    /// on the actual latest attachment, not "images before text".
    pub attachment_order: Vec<AttachmentKind>,
    pub(crate) next_paste_id: u64,
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
    /// First Esc press during a run. A second Esc inside
    /// `INTERRUPT_CONFIRM_WINDOW` performs the cancellation; any other key
    /// disarms it.
    pub interrupt_armed_at: Option<Instant>,
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
    pub pending_task: Option<UserInput>,
    /// Stable receipt for the most recently completed run. Cleared as soon
    /// as the user begins composing the next instruction.
    pub completion: Option<CompletionReceipt>,
    /// Last terminal agent error, rendered as a recovery surface until the
    /// user begins composing again.
    pub last_error: Option<String>,
    /// Full shortcut sheet (`?`) and attachment inspector (`Ctrl+A`) are
    /// transient overlays, never permanent chrome.
    pub shortcuts_open: bool,
    pub attachments_open: bool,
    /// `[ui].reduced_motion` from config. When true: spinner glyph is
    /// static, context-evidence rows skip the dim→normal fade, and
    /// the Run Map active marker doesn't pulse. Streaming coalescing stays
    /// active regardless — it's buffering, not motion.
    pub reduced_motion: bool,
    /// Monotonic counter bumped once per ~100ms UI tick (see `run`'s
    /// `ui_tick`), used only to timestamp when a context-evidence
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
            steering_active: false,
            pending: VecDeque::new(),
            scroll: 0,
            follow: true,
            input: String::new(),
            pasted_attachments: Vec::new(),
            image_attachments: Vec::new(),
            attachment_order: Vec::new(),
            next_paste_id: 1,
            picker: PickerState::default(),
            knowledge: None,
            knowledge_band_open: false,
            repo_dir: String::new(),
            branch: None,
            dirty: false,
            run_started: None,
            current_tool: None,
            tool_started: None,
            interrupt_armed_at: None,
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
            completion: None,
            last_error: None,
            shortcuts_open: false,
            attachments_open: false,
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

    /// Resets per-run state for a freshly submitted task: the Run Map
    /// (objective + steps, with a leading Plan step when `plan_mode` is on),
    /// the Decide-derivation flag, and transient receipts from a prior
    /// run. Pure — the caller still owns emitting the actual task to the
    /// pipeline.
    pub fn start_new_task(&mut self, task: impl Into<UserInput>, plan_mode: bool) {
        let task = task.into();
        self.ledger = LedgerState::new(ledger_objective(&task.text), plan_mode);
        self.steering_active = true;
        self.decide_marked_this_run = false;
        self.tool_started = None;
        self.interrupt_armed_at = None;
        self.response_buf.clear();
        self.md_in_code_block = false;
        self.stream_pending.clear();
        self.stream_last_flush = None;
        self.pending_task = Some(task);
        self.completion = None;
        self.last_error = None;
        self.shortcuts_open = false;
        self.attachments_open = false;
    }

    pub(crate) fn composer_mode(&self) -> ComposerMode {
        if !self.pending.is_empty() {
            ComposerMode::Decision
        } else if self.running && self.steering_active {
            ComposerMode::Steer
        } else if self.running {
            ComposerMode::Queue
        } else if self.last_error.is_some() {
            ComposerMode::Recover
        } else if self.completion.is_some() {
            ComposerMode::FollowUp
        } else {
            ComposerMode::Ask
        }
    }

    pub(crate) fn begin_composing(&mut self) {
        self.completion = None;
        self.last_error = None;
    }

    pub(crate) fn append_pending_steering(&mut self, message: &UserInput) {
        if let Some(task) = self.pending_task.as_mut() {
            let mut steering = message.clone();
            steering.text = format!("[Steering]\n{}", steering.text);
            task.append(steering);
        }
    }

    pub(crate) fn interrupt_confirmation_active(&self) -> bool {
        self.interrupt_armed_at
            .is_some_and(|armed| armed.elapsed() <= INTERRUPT_CONFIRM_WINDOW)
    }

    pub(crate) fn composer_has_content(&self) -> bool {
        !self.input.trim().is_empty()
            || !self.pasted_attachments.is_empty()
            || !self.image_attachments.is_empty()
    }

    pub(crate) fn add_paste(&mut self, pasted: &str) {
        self.begin_composing();
        let normalized = pasted.replace("\r\n", "\n").replace('\r', "\n");
        let normalized: String = normalized
            .chars()
            .filter(|character| *character != '\0')
            .collect();
        let line_count = normalized.split('\n').count();
        let char_count = normalized.chars().count();
        if line_count < PASTE_ATTACHMENT_MIN_LINES && char_count < PASTE_ATTACHMENT_MIN_CHARS {
            self.input.push_str(&normalized);
            return;
        }

        let name = format!("pasted-text-{}.txt", self.next_paste_id);
        self.next_paste_id = self.next_paste_id.saturating_add(1);
        self.pasted_attachments.push(PastedAttachment {
            name,
            content: normalized,
            line_count,
            char_count,
        });
        self.attachment_order.push(AttachmentKind::Text);
    }

    pub(crate) fn add_image_path(&mut self, cwd: &Path, path: &str) -> anyhow::Result<()> {
        if self.image_attachments.len() >= crate::attachments::MAX_IMAGES {
            anyhow::bail!("too many images; maximum is 20 per turn");
        }
        let image = crate::attachments::load_image(cwd, path)?;
        let total = self
            .image_attachments
            .iter()
            .map(|image| image.size_bytes)
            .sum::<usize>()
            .saturating_add(image.size_bytes);
        if total > crate::attachments::MAX_TOTAL_IMAGE_BYTES {
            anyhow::bail!("images exceed the 20 MiB total limit");
        }
        self.begin_composing();
        self.image_attachments.push(image);
        self.attachment_order.push(AttachmentKind::Image);
        Ok(())
    }

    pub(crate) fn attachment_rows(&self) -> Vec<(AttachmentKind, String)> {
        let mut text_index = 0usize;
        let mut image_index = 0usize;
        let mut rows =
            Vec::with_capacity(self.pasted_attachments.len() + self.image_attachments.len());
        for kind in &self.attachment_order {
            match kind {
                AttachmentKind::Text => {
                    if let Some(attachment) = self.pasted_attachments.get(text_index) {
                        rows.push((AttachmentKind::Text, attachment.summary()));
                        text_index += 1;
                    }
                }
                AttachmentKind::Image => {
                    if let Some(image) = self.image_attachments.get(image_index) {
                        rows.push((
                            AttachmentKind::Image,
                            format!(
                                "{} · {} · {:.1} KiB",
                                image.name,
                                image.media_type,
                                image.size_bytes as f64 / 1024.0
                            ),
                        ));
                        image_index += 1;
                    }
                }
            }
        }
        for attachment in &self.pasted_attachments[text_index..] {
            rows.push((AttachmentKind::Text, attachment.summary()));
        }
        for image in &self.image_attachments[image_index..] {
            rows.push((
                AttachmentKind::Image,
                format!(
                    "{} · {} · {:.1} KiB",
                    image.name,
                    image.media_type,
                    image.size_bytes as f64 / 1024.0
                ),
            ));
        }
        rows
    }

    pub(crate) fn remove_last_attachment(&mut self) -> bool {
        let removed = match self.attachment_order.pop() {
            Some(AttachmentKind::Text) => self.pasted_attachments.pop().is_some(),
            Some(AttachmentKind::Image) => self.image_attachments.pop().is_some(),
            None if !self.image_attachments.is_empty() => self.image_attachments.pop().is_some(),
            None => self.pasted_attachments.pop().is_some(),
        };
        if removed {
            self.attachments_open = false;
        }
        removed
    }

    pub(crate) fn take_composer_submission(&mut self) -> UserInput {
        let mut task = std::mem::take(&mut self.input);
        for attachment in std::mem::take(&mut self.pasted_attachments) {
            if !task.is_empty() {
                task.push_str("\n\n");
            }
            task.push_str(&format!(
                "<pasted_text name=\"{}\" lines=\"{}\" chars=\"{}\">\n",
                attachment.name, attachment.line_count, attachment.char_count
            ));
            task.push_str(&attachment.content);
            if !attachment.content.ends_with('\n') {
                task.push('\n');
            }
            task.push_str("</pasted_text>");
        }
        self.attachment_order.clear();
        UserInput {
            text: task,
            images: std::mem::take(&mut self.image_attachments),
        }
    }
}

fn compact_count(count: usize) -> String {
    if count < 1000 {
        count.to_string()
    } else {
        format!("{:.1}k", count as f64 / 1000.0)
    }
}

fn attachment_summary_from_header(header: &str) -> Option<String> {
    if !header.starts_with("<pasted_text ") || !header.ends_with('>') {
        return None;
    }
    let attribute = |name: &str| {
        header
            .split_whitespace()
            .find_map(|part| part.strip_prefix(&format!("{name}=\"")))
            .map(|value| value.trim_end_matches(['\"', '>']))
    };
    let name = attribute("name")?;
    let lines = attribute("lines")?;
    let chars = attribute("chars")?.parse::<usize>().ok()?;
    Some(format!(
        "+ {name} · {lines} lines · {} chars",
        compact_count(chars)
    ))
}

pub(crate) fn user_transcript_lines(task: &str) -> Vec<TranscriptLine> {
    let mut rendered = Vec::new();
    let mut lines = task.lines();
    let mut first = true;
    while let Some(line) = lines.next() {
        if let Some(summary) = attachment_summary_from_header(line) {
            let gutter = if first { Gutter::User } else { Gutter::None };
            rendered.push(TranscriptLine::new(gutter, summary));
            first = false;
            for content_line in lines.by_ref() {
                if content_line == "</pasted_text>" {
                    break;
                }
            }
            continue;
        }
        let gutter = if first { Gutter::User } else { Gutter::None };
        let text = if first {
            line.to_string()
        } else {
            format!("  {line}")
        };
        rendered.push(TranscriptLine::new(gutter, text));
        first = false;
    }
    rendered
}

pub(crate) fn user_input_transcript_lines(input: &UserInput) -> Vec<TranscriptLine> {
    let mut rendered = user_transcript_lines(&input.text);
    for image in &input.images {
        let gutter = if rendered.is_empty() {
            Gutter::User
        } else {
            Gutter::None
        };
        rendered.push(TranscriptLine::new(
            gutter,
            format!(
                "+ {} · {} · {:.1} KiB",
                image.name,
                image.media_type,
                image.size_bytes as f64 / 1024.0
            ),
        ));
    }
    rendered
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
    if let Some(task_input) = state.pending_task.take() {
        let (_, ts) = crate::session::now_utc_stamp();
        let turn = crate::session::Turn {
            ts,
            task: task_input.text,
            images: task_input.images,
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
                    .extend(user_input_transcript_lines(&UserInput {
                        text: t.task.clone(),
                        images: t.images.clone(),
                    }));
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
    let display = user_transcript_lines(task);
    let first_line = display.first().map_or("", |line| line.text.as_str());
    truncate_chars(first_line, 70)
}

/// Applies a lazily-polled `RepoState` (see `spawn_git_poll`) to `state`:
/// the breadcrumb's dirty flag and the Ledger's CURRENT CHANGE numstat
/// rows. Pure — no I/O, called from the `git_rx` arm of the event loop.
pub(crate) fn apply_repo_state(state: &mut AppState, repo: RepoState) {
    state.dirty = repo.dirty;
    state.ledger.numstat = repo.numstat.clone();
    if let Some(receipt) = &mut state.completion {
        receipt.numstat = repo.numstat;
    }
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
