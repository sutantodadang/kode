use std::path::Path;
use std::time::{Duration, Instant};

use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Clear, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap,
};

use kode_core::event::TaskStep;

use super::commands::{BUILTIN_COMMAND_NAMES, picker_filtered_items, slash_hint_items};
use super::markdown;
use super::state::*;
use super::theme;
use crate::custom_commands;

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
    pub(crate) fn plain_text(&self) -> String {
        match self {
            InputSuffix::Counts { z, i, g } => format!("ctx Z:{z} I:{i} G:{g}"),
            InputSuffix::Help => "/help".to_string(),
        }
    }

    /// Styled spans, colored per source (`ctx` label DIM, counts in source
    /// colors) or a plain DIM `/help`.
    pub(crate) fn spans(&self) -> Vec<Span<'static>> {
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
pub(crate) fn engine_status_line(
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
pub(crate) fn empty_state_lines(state: &AppState) -> Vec<Line<'static>> {
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
pub(crate) fn format_k(n: usize) -> String {
    if n < 1000 {
        n.to_string()
    } else {
        format!("{:.1}k", n as f64 / 1000.0)
    }
}

pub(crate) const SPINNER_FRAMES: [char; 4] = ['·', '•', '●', '•'];

/// The single spinner instance's frame for `elapsed_ms`, cycling at 4 Hz
/// (one of the 4 frames every 250ms). Per `DESIGN.md`: ONE moving region
/// max, exactly these frames.
pub(crate) fn spinner_frame(elapsed_ms: u128) -> char {
    let idx = ((elapsed_ms / 250) % 4) as usize;
    SPINNER_FRAMES[idx]
}

/// The spinner glyph to actually render: cycles through `SPINNER_FRAMES` at
/// 4 Hz normally, but holds a single static frame when `reduced_motion` is
/// on (`[ui] reduced_motion`, item 1) or a token stream is actively
/// producing (`streaming`, item 2 — the transcript is the one moving
/// region while tokens flow; the spinner resumes once no tokens are in
/// flight).
pub(crate) fn spinner_glyph(elapsed_ms: u128, reduced_motion: bool, streaming: bool) -> char {
    if reduced_motion || streaming {
        SPINNER_FRAMES[2]
    } else {
        spinner_frame(elapsed_ms)
    }
}

/// Whether buffered stream deltas should flush into the visible transcript
/// now: true on a word/whitespace boundary (the buffer ends in whitespace)
/// or once `elapsed_since_window_start` reaches the 120ms coalescing
/// window — whichever comes first. An empty buffer never flushes. Pure so
/// the coalescing policy (item 2) is unit-testable without a real clock.
pub(crate) fn should_flush_stream_buffer(buf: &str, elapsed_since_window_start: Duration) -> bool {
    if buf.is_empty() {
        return false;
    }
    buf.ends_with(char::is_whitespace) || elapsed_since_window_start >= Duration::from_millis(120)
}

/// Whether a knowledge-band evidence row inserted at `since_tick` should
/// still render dim at `current_tick`: true for its first 2 render ticks,
/// normal from the 3rd (`DESIGN.md`: "New Z/I evidence steps dim→normal
/// over 3 frames"). A row with no recorded insertion tick, or when
/// `reduced_motion` is on, always renders normal. Pure so the fade policy
/// (item 3) is unit-testable without driving the real tick loop.
pub(crate) fn evidence_row_dim(
    current_tick: u64,
    since_tick: Option<u64>,
    reduced_motion: bool,
) -> bool {
    if reduced_motion {
        return false;
    }
    match since_tick {
        Some(t) => current_tick.saturating_sub(t) < 2,
        None => false,
    }
}

/// The Ledger's active-step marker glyph: alternates `●`/`◉` at 4 Hz
/// (250ms period) while a task is running; static `●` when idle or
/// `reduced_motion` is on. Per `DESIGN.md`: active-step marker `●/◉` at
/// 4 Hz (item 4).
pub(crate) fn ledger_pulse_glyph(elapsed_ms: u128, running: bool, reduced_motion: bool) -> char {
    if running && !reduced_motion && !(elapsed_ms / 250).is_multiple_of(2) {
        '◉'
    } else {
        '●'
    }
}

/// Aperture's contraction decision: it never collapses before a
/// `ToolStarted`/`ModelToken` trigger has been seen, and — per `DESIGN.md`
/// motion rules — never earlier than 900ms after it appeared, so it's
/// perceivable even when the trigger fires almost instantly. Pure, so the
/// tick loop's timing behavior is unit-testable without a real clock race.
pub(crate) fn aperture_should_collapse(
    received: Instant,
    now: Instant,
    trigger_seen: bool,
) -> bool {
    trigger_seen && now.saturating_duration_since(received) >= Duration::from_millis(900)
}

/// Maps a `Gutter` to its fixed 2-col glyph prefix and color, per
/// `DESIGN.md`'s glyph vocabulary. Every color pairs with a fixed glyph —
/// shape carries meaning without color.
pub(crate) fn gutter_prefix(gutter: &Gutter) -> (&'static str, Color) {
    match gutter {
        Gutter::None => ("  ", Color::Reset),
        Gutter::Prose => ("│ ", theme::DIM),
        Gutter::Tool => ("T▸", theme::T),
        Gutter::ToolFail => ("T▸", theme::ERR),
        Gutter::Verify => ("V ", theme::OK),
        Gutter::VerifyFail => ("V ", theme::ERR),
        Gutter::VerifySkip => ("V ", theme::DIM),
        Gutter::Note => ("· ", theme::DIM),
        // Per DESIGN.md's decision log: the transcript gutter's Z/I glyphs
        // stay their source colors (cyan/amber), but G renders dim — same
        // literal spec as the item that introduced this gutter, distinct
        // from the Knowledge Band's git-green `theme::G`.
        Gutter::Zindeks => ("Z ", theme::Z),
        Gutter::Ingat => ("I ", theme::I),
        Gutter::Git => ("G ", theme::DIM),
        Gutter::Error => ("× ", theme::ERR),
        Gutter::User => ("U ", Color::Reset),
    }
}

/// Maps a markdown inline style onto a ratatui `Style`, within the existing
/// palette per `DESIGN.md` — no new colors, only bold/dim. Color is
/// provenance, never decoration, so inline code is muted rather than tinted
/// with a source color it doesn't carry.
pub(crate) fn md_span_style(style: &markdown::MdStyle) -> Style {
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
pub(crate) fn transcript_line_to_ratatui(line: &TranscriptLine) -> Line<'static> {
    let (prefix, color) = gutter_prefix(&line.gutter);
    let mut prefix_style = Style::default().fg(color);
    // Source/user letters are bold per DESIGN.md's glyph vocabulary. Agent
    // prose keeps a quiet bar so a multi-line response reads as one block.
    if matches!(
        line.gutter,
        Gutter::Tool
            | Gutter::ToolFail
            | Gutter::Verify
            | Gutter::VerifyFail
            | Gutter::VerifySkip
            | Gutter::Zindeks
            | Gutter::Ingat
            | Gutter::Git
            | Gutter::User
    ) {
        prefix_style = prefix_style.add_modifier(Modifier::BOLD);
    }
    let mut spans = vec![Span::styled(prefix, prefix_style)];
    // A collapsible tool-group header (`tool_children` non-empty) gets a
    // collapse-state glyph ahead of its summary text — `▸` collapsed, `▾`
    // expanded, both already in DESIGN.md's approved vocabulary (the `T▸`
    // gutter, the `▸ {tool}` running label).
    if !line.tool_children.is_empty() {
        let glyph = if line.expanded { "▾ " } else { "▸ " };
        spans.push(Span::styled(glyph, Style::default().fg(theme::DIM)));
    }

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
            let style = if line.gutter == Gutter::User {
                Style::default().add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            spans.push(Span::styled(line.text.clone(), style));
        }
    }
    Line::from(spans)
}

/// Builds the breadcrumb row: `kode  {repo} · {branch} · {provider}/{model}
/// · effort:{e} · ctx ▓▓░░ {used}/{budget}`. `kode` renders dim/lowercase,
/// the rest normal; when no model is selected, a dim ` — /model` nudge is
/// appended after the provider/model cell. The context meter is omitted
/// until the first `Knowledge` event of the session.
pub(crate) fn breadcrumb_line(state: &AppState) -> Line<'static> {
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
        Span::raw(format!("  {} · {branch}", state.repo_dir)),
    ];
    if state.dirty {
        spans.push(Span::styled("*", Style::default().fg(theme::DIM)));
    }
    spans.push(Span::raw(format!(
        " · {}/{model} · effort:{effort}",
        state.status.provider
    )));
    if !model_set {
        spans.push(Span::styled(" — /model", Style::default().fg(theme::DIM)));
    }
    if state.auto_mode {
        spans.push(Span::styled(
            " · auto",
            Style::default().fg(theme::T).add_modifier(Modifier::BOLD),
        ));
    }
    if state.plan_mode {
        spans.push(Span::styled(
            " · plan",
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
/// Splits a trailing `" ┄ 0.87"` confidence suffix (appended by
/// `pipeline::ingat_lines`) off an ingat knowledge line, so it can render
/// dim and separate from the amber/italic ingat text — never baked into
/// the same color, per DESIGN.md ("color = provenance, never decoration").
/// `(line, None)` when no such suffix is present.
pub(crate) fn split_ingat_confidence(line: &str) -> (&str, Option<&str>) {
    match line.rfind(" \u{2504} ") {
        Some(idx) => (&line[..idx], Some(&line[idx + " \u{2504} ".len()..])),
        None => (line, None),
    }
}

pub(crate) fn knowledge_band_lines(
    ks: &KnowledgeState,
    current_tick: u64,
    reduced_motion: bool,
) -> Vec<Line<'static>> {
    let mut lines: Vec<Line> = Vec::new();

    if let Some(first) = ks.zindeks.first() {
        let dim = evidence_row_dim(current_tick, ks.zindeks_since_tick, reduced_motion);
        let glyph_style = if dim {
            Style::default().fg(theme::DIM).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme::Z).add_modifier(Modifier::BOLD)
        };
        let text_style = if dim {
            Style::default().fg(theme::DIM)
        } else {
            Style::default()
        };
        let mut spans = vec![
            Span::raw(" KNOWS  "),
            Span::styled("Z ", glyph_style),
            Span::styled(first.clone(), text_style),
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
        let (text, confidence) = split_ingat_confidence(first);
        let dim = evidence_row_dim(current_tick, ks.ingat_since_tick, reduced_motion);
        let glyph_style = if dim {
            Style::default().fg(theme::DIM).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(theme::I).add_modifier(Modifier::BOLD)
        };
        let text_style = if dim {
            Style::default().fg(theme::DIM)
        } else {
            Style::default().fg(theme::I).add_modifier(Modifier::ITALIC)
        };
        let mut spans = vec![
            Span::raw(" KNOWS  "),
            Span::styled("I ", glyph_style),
            Span::styled(format!("\u{201c}{text}\u{201d}"), text_style),
        ];
        if let Some(score) = confidence {
            spans.push(Span::styled(
                format!(" \u{2504} {score}"),
                Style::default().fg(theme::DIM),
            ));
        }
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

/// One `aperture_lines` row: (source glyph, optional lead-in label, text,
/// text style, optional dim confidence suffix — ingat only).
pub(crate) type ApertureRow = (
    &'static str,
    Option<&'static str>,
    String,
    Style,
    Option<String>,
);

/// Builds the Knowledge Aperture's content lines (not including the
/// trailing rule): a bold header, a request tree of up to the first 2
/// zindeks facts + first ingat memory + first git line (tree connectors
/// `─┬─`/`├─`/`└─`, last present row gets `└─`), then a context summary
/// row. Rows are skipped for empty sources — never a decorative fake.
pub(crate) fn aperture_lines(ks: &KnowledgeState) -> Vec<Line<'static>> {
    let mut rows: Vec<ApertureRow> = Vec::new();
    for z in ks.zindeks.iter().take(2) {
        rows.push(("Z", None, z.clone(), Style::default().fg(theme::Z), None));
    }
    if let Some(entry) = ks.ingat.first() {
        let (text, confidence) = split_ingat_confidence(entry);
        rows.push((
            "I",
            Some("recalled: "),
            format!("\u{201c}{text}\u{201d}"),
            Style::default().fg(theme::I).add_modifier(Modifier::ITALIC),
            confidence.map(|s| s.to_string()),
        ));
    }
    if let Some(git_line) = ks.git.first() {
        rows.push((
            "G",
            None,
            git_line.clone(),
            Style::default().fg(theme::G),
            None,
        ));
    }

    let mut lines = vec![Line::from(Span::styled(
        " KNOWLEDGE APERTURE",
        Style::default()
            .fg(theme::MUTED)
            .add_modifier(Modifier::BOLD),
    ))];

    let last = rows.len().saturating_sub(1);
    for (idx, (src, label, text, style, confidence)) in rows.into_iter().enumerate() {
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
        if let Some(score) = confidence {
            row_spans.push(Span::styled(
                format!(" \u{2504} {score}"),
                Style::default().fg(theme::DIM),
            ));
        }
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
pub(crate) fn draw_knowledge_band(
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

/// Clamps a scroll offset to `[0, total_lines.saturating_sub(viewport_height)]`
/// — the largest offset that still keeps rendered content filling the
/// viewport. Content that fits within the viewport (or a zero-height
/// viewport) always clamps to `0`. Pure and `u16`-safe: never panics on
/// overflow/underflow at the type's edges.
pub(crate) fn clamp_scroll(scroll: u16, total_lines: u16, viewport_height: u16) -> u16 {
    let max_scroll = total_lines.saturating_sub(viewport_height);
    scroll.min(max_scroll)
}

/// Decides whether the transcript scrollbar should render and, if so, the
/// `(content_length, position)` pair for its `ScrollbarState`. Returns
/// `None` when `total_lines` fits within `viewport_height` — the scrollbar
/// auto-hides rather than drawing an inert full-length thumb.
pub(crate) fn scrollbar_state(
    total_lines: u16,
    viewport_height: u16,
    scroll: u16,
) -> Option<(u16, u16)> {
    if total_lines <= viewport_height {
        return None;
    }
    Some((
        total_lines,
        clamp_scroll(scroll, total_lines, viewport_height),
    ))
}

/// Caps a `usize` line count to `u16::MAX` before it feeds `AppState::scroll`
/// or ratatui's `(u16, u16)` scroll offset.
pub(crate) fn lines_as_u16(count: usize) -> u16 {
    count.min(u16::MAX as usize) as u16
}

/// Maps a clicked content row (0-based, already offset by the transcript's
/// current scroll — see `TranscriptHit::scroll`) to the transcript index of
/// the logical line it falls inside, walking `rows`' cumulative wrapped-row
/// counts. `rows` entries are `(wrapped_row_count, transcript_idx)` in
/// render order (see `TranscriptHit::rows`); a logical line spanning
/// multiple wrapped rows maps every one of those rows to the same index.
/// `None` when `content_row` falls past the end of the rendered content, or
/// lands on a line with no transcript index (`None` — prose, plain tool
/// lines, expanded children, the stream line, the spinner label).
pub(crate) fn hit_test_row(rows: &[(u16, Option<usize>)], content_row: u16) -> Option<usize> {
    let mut cursor = 0u16;
    for (count, idx) in rows {
        if content_row < cursor.saturating_add(*count) {
            return *idx;
        }
        cursor = cursor.saturating_add(*count);
    }
    None
}

/// Exact rendered row count for one logical `line` at `width` columns —
/// ratatui's own `Paragraph::line_count` (the `unstable-rendered-line-info`
/// feature, enabled in `crates/kode/Cargo.toml`), run through the identical
/// `Wrap { trim: false }` the transcript actually renders with. Because this
/// calls the same wrapper the `Paragraph` widget uses, the clamp/scrollbar
/// math built on it always agrees with what's on screen — no approximation
/// drift. `width == 0` yields `0` rows (nothing renders).
pub(crate) fn line_rows(line: &Line<'static>, width: u16) -> u16 {
    if width == 0 {
        return 0;
    }
    lines_as_u16(
        Paragraph::new(vec![line.clone()])
            .wrap(Wrap { trim: false })
            .line_count(width),
    )
}

/// Sums `line_rows` across every line of `lines` at `width` columns — the
/// transcript's exact total wrapped-row count, used for scroll clamping and
/// scrollbar sizing.
pub(crate) fn total_wrapped_rows(lines: &[Line<'static>], width: u16) -> usize {
    if width == 0 {
        return 0;
    }
    lines
        .iter()
        .map(|line| line_rows(line, width) as usize)
        .sum()
}

pub(crate) fn draw(f: &mut ratatui::Frame, state: &mut AppState, cwd: &Path) {
    let band_lines = if state.ledger_open {
        // The Ledger view replaces the band + transcript area entirely.
        None
    } else if let Some(ap) = &state.aperture {
        Some(aperture_lines(&ap.knowledge))
    } else if knowledge_band_visible(state) {
        state
            .knowledge
            .as_ref()
            .map(|ks| knowledge_band_lines(ks, state.render_tick, state.reduced_motion))
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
    let hint_items = if !state.picker.open
        && state.pending.is_empty()
        && !state.ledger_open
        && state.input.starts_with('/')
    {
        let custom = custom_commands::discover(cwd, BUILTIN_COMMAND_NAMES);
        slash_hint_items(&state.input, &custom)
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

    let elapsed_ms = state
        .run_started
        .map(|t| t.elapsed())
        .unwrap_or_default()
        .as_millis();

    if state.ledger_open {
        // No transcript rendered while the Ledger is open — nothing to
        // click.
        state.transcript_hit = None;
        draw_ledger(
            f,
            areas[idx],
            &state.ledger,
            state.running,
            elapsed_ms,
            state.reduced_motion,
        );
    } else {
        // Each entry is one logical (pre-wrap) line paired with the
        // `state.transcript` index to toggle on click, when it's an
        // expandable tool-group header (`Some`) — everything else
        // (prose, plain tool lines, expanded children, the stream line,
        // the spinner label) is `None`.
        let mut entries: Vec<(Line<'static>, Option<usize>)> = Vec::new();
        if show_empty_state(&state.transcript, state.running) {
            entries.extend(empty_state_lines(state).into_iter().map(|l| (l, None)));
        }
        for (i, line) in state.transcript.iter().enumerate() {
            let is_header = !line.tool_children.is_empty();
            entries.push((
                transcript_line_to_ratatui(line),
                if is_header { Some(i) } else { None },
            ));
            if is_header && line.expanded {
                for child in &line.tool_children {
                    let child_line = TranscriptLine::new(Gutter::Tool, format!("  {child}"));
                    entries.push((transcript_line_to_ratatui(&child_line), None));
                }
            }
        }
        if !state.current_stream.is_empty() {
            entries.push((
                transcript_line_to_ratatui(&TranscriptLine::new(
                    Gutter::Prose,
                    state.current_stream.clone(),
                )),
                None,
            ));
        }
        if state.running {
            let elapsed = state.run_started.map(|t| t.elapsed()).unwrap_or_default();
            let streaming = !state.current_stream.is_empty() || !state.stream_pending.is_empty();
            let frame = spinner_glyph(elapsed.as_millis(), state.reduced_motion, streaming);
            let secs = elapsed.as_secs_f64();
            let (label, color) = if state.interrupt_confirmation_active() {
                ("× press Esc again to interrupt".to_string(), theme::ERR)
            } else {
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
                (label, theme::T)
            };
            entries.push((
                Line::from(Span::styled(label, Style::default().fg(color))),
                None,
            ));
        }
        let (text_lines, indices): (Vec<Line>, Vec<Option<usize>>) = entries.into_iter().unzip();
        let transcript_area = areas[idx];

        // Decide, at the full transcript width, whether a scrollbar column
        // needs reserving. If it does, the text area narrows by one column
        // — re-measure at that narrower width so wrap/clamp/scrollbar/hit-
        // test math all agree with what's actually rendered (narrowing can
        // only add wrapped lines, never remove the overflow, so this never
        // flaps).
        let total_lines_full = lines_as_u16(total_wrapped_rows(&text_lines, transcript_area.width));
        let scrollbar_needed = total_lines_full > transcript_area.height;
        let (text_area, scrollbar_area) = if scrollbar_needed && transcript_area.width > 1 {
            let cols = Layout::horizontal([Constraint::Min(1), Constraint::Length(1)])
                .split(transcript_area);
            (cols[0], Some(cols[1]))
        } else {
            (transcript_area, None)
        };
        let total_lines = if scrollbar_area.is_some() {
            lines_as_u16(total_wrapped_rows(&text_lines, text_area.width))
        } else {
            total_lines_full
        };
        let viewport_height = text_area.height;

        // Following pins to the bottom every frame so new content stays in
        // view; otherwise the user's position is clamped to stay in range.
        state.scroll = if state.follow {
            total_lines.saturating_sub(viewport_height)
        } else {
            clamp_scroll(state.scroll, total_lines, viewport_height)
        };

        // Per-line row counts at the width actually rendered, paired with
        // each line's transcript index (if it's a clickable group header)
        // — `handle_mouse`'s click hit-test walks this.
        let rows: Vec<(u16, Option<usize>)> = text_lines
            .iter()
            .zip(indices.iter())
            .map(|(l, idx)| (line_rows(l, text_area.width), *idx))
            .collect();
        state.transcript_hit = Some(TranscriptHit {
            area: text_area,
            scroll: state.scroll,
            rows,
        });

        let transcript = Paragraph::new(text_lines)
            .wrap(Wrap { trim: false })
            .block(Block::default().borders(Borders::NONE))
            .scroll((state.scroll, 0));
        f.render_widget(transcript, text_area);

        if let Some(area) = scrollbar_area
            && let Some((content_len, position)) =
                scrollbar_state(total_lines, viewport_height, state.scroll)
        {
            let mut sb_state =
                ScrollbarState::new(content_len as usize).position(position as usize);
            let bar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .begin_symbol(None)
                .end_symbol(None)
                .track_symbol(Some("│"))
                .thumb_symbol("│")
                .track_style(Style::default().fg(theme::DIM))
                .thumb_style(Style::default());
            f.render_stateful_widget(bar, area, &mut sb_state);
        }
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
pub(crate) fn slash_hint_lines(items: &[(String, String)], selected: usize) -> Vec<Line<'static>> {
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
/// Right-edge input-line indicator while Ctrl+T select mode is on (mouse
/// released to the terminal so native drag-select/copy works).
pub(crate) const SELECT_MODE_HINT: &str = "select · Ctrl+T to exit  ";

pub(crate) fn draw_input(f: &mut ratatui::Frame, area: ratatui::layout::Rect, state: &AppState) {
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
    let select_text = if state.select_mode {
        SELECT_MODE_HINT
    } else {
        ""
    };
    let suffix_len = suffix.plain_text().chars().count() + select_text.chars().count();
    let prefix_width = 3 + state.input.chars().count(); // " › " + input
    let pad = (line_area.width as usize).saturating_sub(prefix_width + suffix_len);

    let mut spans = vec![
        Span::styled(" › ", Style::default().fg(theme::MUTED)),
        Span::raw(state.input.clone()),
        Span::raw(" ".repeat(pad)),
    ];
    if state.select_mode {
        spans.push(Span::styled(select_text, Style::default().fg(theme::T)));
    }
    spans.extend(suffix.spans());
    f.render_widget(Paragraph::new(Line::from(spans)), line_area);

    if !state.picker.open && state.pending.is_empty() {
        let cursor_x = (line_area.x as usize + 3 + state.input.chars().count())
            .min((line_area.x + line_area.width).saturating_sub(1) as usize)
            as u16;
        f.set_cursor_position((cursor_x, line_area.y));
    }
}

pub(crate) fn task_step_label(step: TaskStep) -> &'static str {
    match step {
        TaskStep::Plan => "PLAN",
        TaskStep::Understand => "UNDERSTAND",
        TaskStep::Decide => "DECIDE",
        TaskStep::Change => "CHANGE",
        TaskStep::Verify => "VERIFY",
    }
}

/// Renders the Ledger view (Ctrl+L): OBJECTIVE, the 4 numbered steps
/// (`✓` done / `●`/`◉` active pulse / `○` pending — no borders, DIM rule
/// spacing only), CURRENT CHANGE, and WHY. Every row traces to a real
/// event; no invented captions. `running`/`elapsed_ms`/`reduced_motion`
/// drive the active marker's 4 Hz pulse (item 4).
pub(crate) fn draw_ledger(
    f: &mut ratatui::Frame,
    area: ratatui::layout::Rect,
    ledger: &LedgerState,
    running: bool,
    elapsed_ms: u128,
    reduced_motion: bool,
) {
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
            let g: &'static str = if ledger_pulse_glyph(elapsed_ms, running, reduced_motion) == '◉'
            {
                "◉"
            } else {
                "●"
            };
            (
                g,
                Style::default()
                    .fg(theme::MUTED)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            ("○", Style::default().fg(theme::DIM))
        };

        let caption = match step {
            TaskStep::Change => numstat_caption(&ledger.numstat),
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
    if ledger.numstat.is_empty() {
        lines.push(Line::from(Span::styled(
            "   no edits yet",
            Style::default().fg(theme::DIM),
        )));
    } else {
        const MAX_ROWS: usize = 3;
        for row in ledger.numstat.iter().take(MAX_ROWS) {
            // Diff isn't a Z/I/G provenance source, so per DESIGN.md ("color
            // = provenance, never decoration") the whole row stays dim/plain
            // — only the `+`/`-` glyphs (already in the vocabulary) carry
            // meaning, not color.
            lines.push(Line::from(vec![
                Span::raw(format!("   {}  ", row.path)),
                Span::styled(
                    format!("+{} -{}", row.added, row.deleted),
                    Style::default().fg(theme::DIM),
                ),
            ]));
        }
        if ledger.numstat.len() > MAX_ROWS {
            lines.push(Line::from(Span::styled(
                format!("   +{} more", ledger.numstat.len() - MAX_ROWS),
                Style::default().fg(theme::DIM),
            )));
        }
    }

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
pub(crate) fn draw_picker(f: &mut ratatui::Frame, picker: &PickerState) {
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
        PickerKind::Session => "resume session",
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
