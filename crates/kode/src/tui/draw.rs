use std::collections::VecDeque;
use std::path::Path;
use std::time::{Duration, Instant};

use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Clear, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap,
};
use unicode_width::UnicodeWidthStr;

use kode_core::event::TaskStep;

use super::commands::{picker_filtered_items, slash_hint_items};
use super::markdown;
use super::state::*;
use super::theme;

/// Renders an 8-cell (by default) meter string: `■` for filled cells,
/// `□` for empty ones. `budget == 0` yields an all-empty meter (no
/// division by zero). Pure — the caller applies default/DIM coloring per
/// cell.
pub fn meter(used: usize, budget: usize, cells: usize) -> String {
    let filled = if budget == 0 {
        0
    } else {
        let ratio = used as f64 / budget as f64;
        ((ratio * cells as f64).round() as usize).min(cells)
    };
    let mut s = String::with_capacity(cells * 3);
    for i in 0..cells {
        s.push(if i < filled { '■' } else { '□' });
    }
    s
}

/// True when the Knowledge Band should render: the user hasn't collapsed
/// it (Ctrl+K), a `Knowledge` event has arrived, and at least one source
/// has data. Per `DESIGN.md`: never fake provenance — the band is hidden
/// entirely when a source is unavailable/empty, never shown padded/empty.
#[cfg(test)]
pub fn knowledge_band_visible(state: &AppState) -> bool {
    state.knowledge_band_open
        && state
            .knowledge
            .as_ref()
            .is_some_and(|k| !k.zindeks.is_empty() || !k.ingat.is_empty() || !k.git.is_empty())
}

/// True when the transcript area should render the idle empty-state block
/// (version/tagline, engine status, input nudge) instead of the normal
/// transcript. This is the idle screen — it stays true while nothing but startup `Note` hints (model
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
#[cfg(test)]
pub enum InputSuffix {
    Counts { z: usize, i: usize, g: usize },
    Help,
}

#[cfg(test)]
impl InputSuffix {
    /// Plain-text rendering, used to size the row for right-alignment.
    pub(crate) fn plain_text(&self) -> String {
        match self {
            InputSuffix::Counts { z, i, g } => format!("ctx Z:{z} I:{i} G:{g}"),
            InputSuffix::Help => "/help".to_string(),
        }
    }
}

/// Decides the input line's suffix from the session's last Knowledge
/// digest (`None` until the first context compilation of the session).
#[cfg(test)]
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

/// Display width of a span list, in terminal cells.
fn spans_width(spans: &[Span<'_>]) -> usize {
    spans
        .iter()
        .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
        .sum()
}

/// One idle empty-state engine-status body: `{label}: {status word}`.
/// `disabled` is a caution (`WARN`); `available after first task` stays dim.
pub(crate) fn engine_status_spans(
    label: &str,
    enabled: bool,
    source_seen: bool,
    ready_color: Color,
) -> Vec<Span<'static>> {
    let (text, style) = match engine_status(enabled, source_seen) {
        EngineStatus::Disabled => ("disabled".to_string(), Style::default().fg(theme::WARN)),
        EngineStatus::Ready => ("ready".to_string(), Style::default().fg(ready_color)),
        EngineStatus::AvailableAfterFirstTask => (
            "available after first task".to_string(),
            Style::default().fg(theme::DIM),
        ),
    };
    vec![
        Span::styled(format!("{label}: "), Style::default().fg(theme::MUTED)),
        Span::styled(text, style),
    ]
}

/// Builds the idle empty-state block on the thread gutter: column heads,
/// one knot row per engine (graph, memory, git), then the input nudge.
/// Top-left anchored, one blank row down — never vertically centered.
/// `width` picks the wide (7-cell)
/// or narrow (3-cell) gutter.
pub(crate) fn empty_state_lines(state: &AppState, width: u16) -> Vec<Line<'static>> {
    let wide = is_wide(width);
    let zindeks_seen = state
        .knowledge
        .as_ref()
        .is_some_and(|k| !k.zindeks.is_empty());
    let ingat_seen = state
        .knowledge
        .as_ref()
        .is_some_and(|k| !k.ingat.is_empty());
    let git_body = vec![
        Span::styled("git: ", Style::default().fg(theme::MUTED)),
        if state.dirty {
            Span::styled("worktree has changes", Style::default().fg(theme::WARN))
        } else {
            Span::raw("worktree clean")
        },
    ];
    let row = |gutter: Gutter, body: Vec<Span<'static>>, name: &str, color: Color| {
        let mut spans = gutter_spans(gutter, false, wide);
        spans.extend(body);
        if wide {
            let used = spans_width(&spans);
            let name_w = name.chars().count();
            if used + name_w + 2 <= width as usize {
                spans.push(Span::raw(" ".repeat(width as usize - used - name_w - 1)));
                spans.push(Span::styled(name.to_string(), Style::default().fg(color)));
            }
        }
        Line::from(spans)
    };
    let mut lines = vec![Line::default(), thread_head_line(wide)];
    lines.push(row(
        Gutter::Zindeks,
        engine_status_spans("code graph", state.zindeks_enabled, zindeks_seen, theme::Z),
        "graph",
        theme::Z,
    ));
    lines.push(row(
        Gutter::Ingat,
        engine_status_spans("memory", state.ingat_enabled, ingat_seen, theme::I),
        "memory",
        theme::I,
    ));
    lines.push(row(Gutter::Git, git_body, "git", theme::G));
    lines.push(Line::from(gutter_spans(Gutter::Prose, false, wide)));
    lines.push(Line::from(vec![
        Span::raw(if wide { "        " } else { "   " }),
        Span::styled(
            "Type a task, or press / for commands and ? for help.",
            Style::default().fg(theme::MUTED),
        ),
    ]));
    if !state.status.model.is_empty() {
        return lines;
    }
    lines.push(Line::from(Span::styled(
        " Pick a model with /model before submitting.",
        Style::default().fg(theme::WARN),
    )));
    lines
}

pub(crate) const SPINNER_FRAMES: [char; 4] = ['◐', '◓', '◑', '◒'];

fn compact_tokens(tokens: usize) -> String {
    if tokens < 1000 {
        tokens.to_string()
    } else {
        format!("{:.1}k", tokens as f64 / 1000.0)
    }
}

fn cached_label(cached: Option<u64>) -> String {
    match cached {
        Some(tokens) => format!("{} cached", compact_tokens(tokens as usize)),
        None => "cached ? not reported".to_string(),
    }
}

/// The running glyph advances every 250 ms (4 frames per turn); every
/// running glyph shares this one phase.
pub(crate) fn spinner_frame(elapsed_ms: u128) -> char {
    let idx = ((elapsed_ms / 250) % 4) as usize;
    SPINNER_FRAMES[idx]
}

/// The spinner glyph to actually render: cycles through `SPINNER_FRAMES` at
/// 250 ms per frame normally, but holds a single static frame when `reduced_motion` is
/// on (`[ui] reduced_motion`) or a token stream is actively producing
/// (`streaming` — the now-line shows `▸` then; the glyph resumes once no
/// tokens are in flight).
pub(crate) fn spinner_glyph(elapsed_ms: u128, reduced_motion: bool, streaming: bool) -> char {
    if reduced_motion || streaming {
        SPINNER_FRAMES[0]
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
/// normal from the 3rd. A row with no recorded insertion tick, or when
/// `reduced_motion` is on, always renders normal. Pure so the fade policy
/// is unit-testable without driving the real tick loop.
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

/// The Run Map's active-step marker alternates at 1 Hz while a task is
/// running; static `●` when idle or
/// `reduced_motion` is on.
pub(crate) fn ledger_pulse_glyph(elapsed_ms: u128, running: bool, reduced_motion: bool) -> char {
    if running && !reduced_motion && !(elapsed_ms / 1000).is_multiple_of(2) {
        '◉'
    } else {
        '●'
    }
}

/// Wide layout (7-cell thread gutter, step-rail names, ctx meter) applies
/// from 80 columns up; below that the gutter collapses to 3 cells.
pub(crate) fn is_wide(width: u16) -> bool {
    width >= 80
}

/// True when a tool line names an edit-class tool (`edit`, `write`,
/// `patch`), which draws as a knot on the thread rather than a plain run.
fn is_edit_tool(line: &TranscriptLine) -> bool {
    const EDIT_WORDS: [&str; 3] = ["edit", "write", "patch"];
    std::iter::once(&line.text)
        .chain(line.tool_children.iter())
        .any(|name| {
            let name = name.to_ascii_lowercase();
            EDIT_WORDS.iter().any(|word| name.contains(word))
        })
}

/// The gutter cells for one transcript line, per `DESIGN.md`'s thread
/// vocabulary. `wide` selects the 7-cell three-thread gutter (graph, memory,
/// git) over the 3-cell one. `edit` marks an edit-class tool. Prose, notes
/// and errors carry idle DIM threads; `None`/`User` carry no gutter.
pub(crate) fn gutter_spans(gutter: Gutter, edit: bool, wide: bool) -> Vec<Span<'static>> {
    let dim = Style::default().fg(theme::DIM);
    let bold = Style::default().add_modifier(Modifier::BOLD);
    let run = |c: Color| Style::default().fg(c);
    let knot = |c: Color| Style::default().fg(c).add_modifier(Modifier::BOLD);
    let span = |text: &'static str, style: Style| Span::styled(text, style);
    let edit_style = if gutter == Gutter::ToolFail {
        knot(theme::ERR)
    } else {
        bold
    };
    match (gutter, wide) {
        (Gutter::None | Gutter::User | Gutter::Reply | Gutter::ThreadHead, _) => Vec::new(),
        (Gutter::Zindeks, true) => vec![span("●", knot(theme::Z)), span("─┼─┼─ ", run(theme::Z))],
        (Gutter::Ingat, true) => vec![
            span("│ ", dim),
            span("●", knot(theme::I)),
            span("─┼─ ", run(theme::I)),
        ],
        (Gutter::Git, true) => vec![
            span("│ │ ", dim),
            span("●", knot(theme::G)),
            span("─ ", run(theme::G)),
        ],
        (Gutter::Zindeks, false) => vec![span("g●", knot(theme::Z)), span(" ", dim)],
        (Gutter::Ingat, false) => vec![span("m●", knot(theme::I)), span(" ", dim)],
        (Gutter::Git, false) => vec![span("t●", knot(theme::G)), span(" ", dim)],
        (Gutter::Route, true) => vec![span("╰─┴─┴▶ ", bold)],
        (Gutter::Route, false) => vec![span("▶  ", bold)],
        (Gutter::Tool | Gutter::ToolFail, true) if edit => vec![span("●═╪═╪═ ", edit_style)],
        (Gutter::Tool | Gutter::ToolFail, false) if edit => vec![span("●═ ", edit_style)],
        (Gutter::Verify | Gutter::VerifyFail | Gutter::VerifySkip, true) => {
            vec![span("┆ ┆ ┆  ", dim)]
        }
        (_, true) => vec![span("│ │ │  ", dim)],
        (_, false) => vec![span("   ", dim)],
    }
}

/// The `g m t` column-head row (each letter bold in its source color) drawn
/// once before a run's first graph/memory/git line. Empty at narrow width,
/// where each source line carries its own letter instead.
fn thread_head_line(wide: bool) -> Line<'static> {
    if !wide {
        return Line::default();
    }
    let head = |letter: &'static str, c: Color| {
        Span::styled(letter, Style::default().fg(c).add_modifier(Modifier::BOLD))
    };
    Line::from(vec![
        head("g", theme::Z),
        Span::raw(" "),
        head("m", theme::I),
        Span::raw(" "),
        head("t", theme::G),
    ])
}

/// Result glyph (and word, for skips) plus color for one check status:
/// passed `✓`, failed `✗`, skipped `⊘ skipped`.
fn check_mark(status: StepStatusLite) -> (&'static str, Color) {
    match status {
        StepStatusLite::Passed => ("✓", theme::OK),
        StepStatusLite::Failed => ("✗", theme::ERR),
        StepStatusLite::Skipped => ("⊘ skipped", theme::WARN),
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

/// Cells of the thread run revealed after the knot: 0..=3 over 3 frames of
/// 40 ms. Reduced motion shows the final state.
pub(crate) fn thread_pull_frame(elapsed_ms: u128, reduced_motion: bool) -> u8 {
    if reduced_motion {
        return 3;
    }
    (elapsed_ms / 40).min(3) as u8
}

/// Route gutter cells revealed (0..=6) over 4 frames of 50 ms.
pub(crate) fn converge_cells(elapsed_ms: u128, reduced_motion: bool) -> usize {
    if reduced_motion {
        return 6;
    }
    let frame = (elapsed_ms / 50).min(4) as usize;
    frame * 6 / 4
}

/// Filled bar cells shown: 0 until 200 ms, then one more every 30 ms, capped
/// at `filled`.
pub(crate) fn bar_fill_cells(elapsed_ms: u128, filled: usize, reduced_motion: bool) -> usize {
    if reduced_motion {
        return filled;
    }
    if elapsed_ms < 200 {
        return 0;
    }
    let steps = (elapsed_ms - 200) / 30 + 1;
    steps.min(filled as u128) as usize
}

/// Connector after a finished step: 1 heavy cell before 90 ms, 2 after.
pub(crate) fn rail_fill_cells(elapsed_ms: u128, reduced_motion: bool) -> usize {
    if reduced_motion || elapsed_ms >= 90 {
        2
    } else {
        1
    }
}

/// Sparkline level 0..=6 for `tokens` against the run's `peak` (0 tokens or
/// peak 0 => 0; any tokens show at least level 1).
pub(crate) fn pulse_level(tokens: u32, peak: u32) -> usize {
    if tokens == 0 || peak == 0 {
        return 0;
    }
    ((u64::from(tokens) * 6 / u64::from(peak)) as usize).clamp(1, 6)
}

/// Motion window: a line born less than this ago still needs per-frame
/// re-rendering (longest animation is the bar fill, ~500 ms).
const LINE_ANIMATION_MS: u128 = 600;

/// True while a line born at `born` still needs per-frame re-rendering.
/// Lines without a birth time (and reduced motion) are always final.
pub(crate) fn line_animating(born: Option<Instant>, now: Instant, reduced_motion: bool) -> bool {
    if reduced_motion {
        return false;
    }
    born.is_some_and(|b| now.saturating_duration_since(b).as_millis() < LINE_ANIMATION_MS)
}

/// The token-pulse sparkline: the last `max_cells` cells. `Tokens` scale
/// against the peak of the whole deque; `Tool` is `─`, `Wait` is `·`.
pub(crate) fn pulse_spans(cells: &VecDeque<PulseCell>, max_cells: usize) -> Vec<Span<'static>> {
    const LEVELS: [char; 7] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇'];
    let peak = cells
        .iter()
        .filter_map(|c| match c {
            PulseCell::Tokens(n) => Some(*n),
            _ => None,
        })
        .max()
        .unwrap_or(0);
    let skip = cells.len().saturating_sub(max_cells);
    cells
        .iter()
        .skip(skip)
        .map(|cell| match cell {
            PulseCell::Tokens(n) => Span::raw(LEVELS[pulse_level(*n, peak)].to_string()),
            PulseCell::Tool => Span::styled("─", Style::default().fg(theme::T)),
            PulseCell::Wait => Span::styled("·", Style::default().fg(theme::WARN)),
        })
        .collect()
}

/// Idle thread pattern per gutter cell, shown where motion has not yet
/// revealed the final cell.
const IDLE_CELLS: [char; 7] = ['│', ' ', '│', ' ', '│', ' ', ' '];

/// Replaces the gutter cells at positions where `hide` is true with the idle
/// thread pattern in `theme::DIM`.
fn mask_gutter(spans: Vec<Span<'static>>, hide: impl Fn(usize) -> bool) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    for span in spans {
        for c in span.content.chars() {
            if hide(pos) {
                let idle = IDLE_CELLS.get(pos).copied().unwrap_or(' ');
                out.push(Span::styled(
                    idle.to_string(),
                    Style::default().fg(theme::DIM),
                ));
            } else {
                out.push(Span::styled(c.to_string(), span.style));
            }
            pos += 1;
        }
    }
    out
}

/// Spans for a route line's text: a `■□` bar fills over time and the score
/// stays hidden until the bar is full; `□` cells are DIM and a trailing
/// ` low` is WARN. A route without a bar is one bold span.
fn route_text_spans(text: &str, elapsed_ms: u128, reduced_motion: bool) -> Vec<Span<'static>> {
    let bold = Style::default().add_modifier(Modifier::BOLD);
    let chars: Vec<char> = text.chars().collect();
    let is_bar = |c: &char| matches!(c, '■' | '□');
    let Some(start) = chars.iter().position(is_bar) else {
        return vec![Span::styled(text.to_string(), bold)];
    };
    let len = chars[start..].iter().take_while(|c| is_bar(c)).count();
    let filled = chars[start..start + len]
        .iter()
        .filter(|c| **c == '■')
        .count();
    let shown = bar_fill_cells(elapsed_ms, filled, reduced_motion);
    let mut out = vec![
        Span::styled(chars[..start].iter().collect::<String>(), bold),
        Span::styled("■".repeat(shown), bold),
        Span::styled("□".repeat(len - shown), Style::default().fg(theme::DIM)),
    ];
    if shown >= filled && (reduced_motion || elapsed_ms >= 200) {
        let rest: String = chars[start + len..].iter().collect();
        match rest.strip_suffix(" low") {
            Some(head) => {
                out.push(Span::styled(head.to_string(), bold));
                out.push(Span::styled(" low", Style::default().fg(theme::WARN)));
            }
            None => out.push(Span::styled(rest, bold)),
        }
    }
    out
}

/// Renders one transcript line as thread-gutter cells + text span(s) at the
/// given transcript `width` (which picks the wide or narrow gutter).
/// Markdown-rendered Prose lines (`md_kind`/`spans` both `Some`) render their
/// styled spans; everything else falls back to the legacy plain-text span.
pub(crate) fn transcript_line_to_ratatui(line: &TranscriptLine, width: u16) -> Line<'static> {
    transcript_line_at(line, width, Instant::now(), false, false)
}

/// [`transcript_line_to_ratatui`] with the Kode Benang motion inputs: `now`
/// and `reduced_motion` drive the thread pull / route converge (pure
/// functions of time since `line.born`), `trace_bold` bolds a fact line's
/// text for the trace-back.
pub(crate) fn transcript_line_at(
    line: &TranscriptLine,
    width: u16,
    now: Instant,
    reduced_motion: bool,
    trace_bold: bool,
) -> Line<'static> {
    let wide = is_wide(width);
    let elapsed = match line.born {
        Some(born) if !reduced_motion => now.saturating_duration_since(born).as_millis(),
        _ => u128::MAX,
    };
    match line.gutter {
        Gutter::ThreadHead => return thread_head_line(wide),
        Gutter::None if line.text.is_empty() => return Line::default(),
        Gutter::User => {
            let bold = Style::default().add_modifier(Modifier::BOLD);
            return Line::from(vec![
                Span::styled("YOU", bold),
                Span::raw("  "),
                Span::styled(line.text.clone(), bold),
            ]);
        }
        Gutter::Reply => {
            return Line::from(Span::styled(
                "KODE",
                Style::default().add_modifier(Modifier::BOLD),
            ));
        }
        _ => {}
    }
    let edit = matches!(line.gutter, Gutter::Tool | Gutter::ToolFail) && is_edit_tool(line);
    let mut spans = gutter_spans(line.gutter, edit, wide);
    if wide {
        // (knot cell, run length) of each source thread.
        let fact = match line.gutter {
            Gutter::Zindeks => Some((0usize, 5usize)),
            Gutter::Ingat => Some((2, 3)),
            Gutter::Git => Some((4, 1)),
            _ => None,
        };
        if let Some((knot, run)) = fact {
            let frame = thread_pull_frame(elapsed, reduced_motion);
            if frame < 3 {
                let revealed = run * usize::from(frame) / 3;
                let spans = mask_gutter(spans, |pos| pos > knot + revealed && pos <= knot + run);
                return Line::from(spans);
            }
        } else if line.gutter == Gutter::Route {
            let cells = converge_cells(elapsed, reduced_motion);
            if cells < 6 {
                spans = mask_gutter(spans, |pos| pos >= cells);
            }
        }
    }
    if line.gutter == Gutter::Error {
        spans.push(Span::styled(
            "FAILED ",
            Style::default().fg(theme::ERR).add_modifier(Modifier::BOLD),
        ));
    }
    // A collapsible tool-group header (`tool_children` non-empty) gets a
    // collapse-state glyph ahead of its summary text — `▸` collapsed, `▾`
    // expanded. Other non-edit tool lines get the plain `▸` tool marker.
    if !line.tool_children.is_empty() {
        let glyph = if line.expanded { "▾ " } else { "▸ " };
        spans.push(Span::styled(glyph, Style::default().fg(theme::DIM)));
    } else if matches!(line.gutter, Gutter::Tool | Gutter::ToolFail) && !edit {
        let color = if line.gutter == Gutter::ToolFail {
            theme::ERR
        } else {
            theme::T
        };
        spans.push(Span::styled("▸ ", Style::default().fg(color)));
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
        _ if line.gutter == Gutter::Route => {
            spans.extend(route_text_spans(&line.text, elapsed, reduced_motion));
        }
        _ => {
            let fact = matches!(line.gutter, Gutter::Zindeks | Gutter::Ingat | Gutter::Git);
            let style = if trace_bold && fact {
                Style::default().add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            spans.push(Span::styled(line.text.clone(), style));
        }
    }
    if line.gutter == Gutter::Tool
        && let Some(ok) = line.tool_ok
    {
        let duration = line.tool_duration_ms.unwrap_or(0) as f64 / 1000.0;
        let (mark, color) = if ok {
            ("✓", theme::OK)
        } else {
            ("✗", theme::ERR)
        };
        spans.push(Span::styled(
            format!(" · {duration:.1}s {mark}"),
            Style::default().fg(color),
        ));
    }
    let verify_status = match line.gutter {
        Gutter::Verify => Some(StepStatusLite::Passed),
        Gutter::VerifyFail => Some(StepStatusLite::Failed),
        Gutter::VerifySkip => Some(StepStatusLite::Skipped),
        _ => None,
    };
    if let Some(status) = verify_status {
        let (mark, color) = check_mark(status);
        spans.push(Span::styled(format!(" {mark}"), Style::default().fg(color)));
    }
    // Right-align the source name on graph/memory/git lines when the text
    // fits on one row; a wrapped line omits it.
    let source = match line.gutter {
        Gutter::Zindeks => Some(("graph", theme::Z)),
        Gutter::Ingat => Some(("memory", theme::I)),
        Gutter::Git => Some(("git", theme::G)),
        _ => None,
    };
    if wide
        && line.md_kind.is_none()
        && let Some((name, color)) = source
    {
        let used = spans_width(&spans);
        let name_w = name.len();
        if used + name_w + 2 <= width as usize {
            spans.push(Span::raw(" ".repeat(width as usize - used - name_w - 1)));
            spans.push(Span::styled(name, Style::default().fg(color)));
        }
    }
    Line::from(spans)
}

/// `m:ss` for an elapsed millisecond count.
fn fmt_mss(ms: u128) -> String {
    let secs = ms / 1000;
    format!("{}:{:02}", secs / 60, secs % 60)
}

/// Live counter text: `{:.1}s`, or whole seconds under `reduced_motion` so
/// the row doesn't repaint every tick.
fn fmt_counter(ms: u128, reduced_motion: bool) -> String {
    if reduced_motion {
        format!("{}s", ms / 1000)
    } else {
        format!("{:.1}s", ms as f64 / 1000.0)
    }
}

/// Builds the stable scope row: repo, branch, run authority (`PLAN`/`AUTO`),
/// model, and a right-aligned context meter. When space is short the model
/// drops first; authority labels never drop.
pub(crate) fn scope_line(state: &AppState, width: u16) -> Line<'static> {
    let w = width as usize;
    let branch = state.branch.clone().unwrap_or_else(|| "no git".to_string());
    let repo = if state.repo_dir.is_empty() {
        "."
    } else {
        state.repo_dir.as_str()
    };
    let mut left = vec![
        Span::raw(" "),
        Span::styled(
            repo.to_string(),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!(" ⎇ {branch}")),
    ];
    if state.dirty {
        left.push(Span::styled("*", Style::default().fg(theme::WARN)));
    }
    for (on, label) in [(state.plan_mode, "PLAN"), (state.auto_mode, "AUTO")] {
        if on {
            left.push(Span::raw("  "));
            left.push(Span::styled(
                label,
                Style::default()
                    .fg(theme::WARN)
                    .add_modifier(Modifier::BOLD),
            ));
        }
    }
    let model = vec![
        Span::raw("  "),
        if state.status.model.is_empty() {
            Span::styled("pick model", Style::default().fg(theme::WARN))
        } else {
            Span::styled(
                state.status.model.clone(),
                Style::default().fg(theme::MUTED),
            )
        },
    ];

    let muted = Style::default().fg(theme::MUTED);
    let right: Vec<Span<'static>> = match &state.knowledge {
        None => vec![Span::styled("ctx —", muted)],
        Some(ks) => {
            let numbers = format!(
                "{}/{}",
                compact_tokens(ks.context_tokens),
                compact_tokens(ks.budget_tokens)
            );
            if is_wide(width) {
                let bar = meter(ks.context_tokens, ks.budget_tokens, 8);
                let filled: String = bar.chars().filter(|c| *c == '■').collect();
                let empty: String = bar.chars().filter(|c| *c == '□').collect();
                vec![
                    Span::styled("ctx ", muted),
                    Span::raw(filled),
                    Span::styled(empty, Style::default().fg(theme::DIM)),
                    Span::styled(format!(" {numbers}"), muted),
                ]
            } else {
                vec![Span::styled(format!("ctx {numbers}"), muted)]
            }
        }
    };

    let mut used = spans_width(&left) + spans_width(&right);
    let model_w = spans_width(&model);
    let mut spans = left;
    if used + model_w < w {
        spans.extend(model);
        used += model_w;
    }
    spans.push(Span::raw(" ".repeat(w.saturating_sub(used))));
    spans.extend(right);
    Line::from(spans)
}

#[cfg(test)]
pub(crate) fn breadcrumb_line(state: &AppState) -> Line<'static> {
    scope_line(state, 120)
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
/// lines, expanded children, the stream line, the now-line).
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

/// Reading measure for model prose, in text columns after the gutter.
pub(crate) const PROSE_MAX_COLS: usize = 100;

/// Spans that open every wrapped row after the first: the idle thread gutter
/// (verification keeps its dotted threads), plus a hanging indent under a
/// bullet's text or after `YOU  `.
fn continuation_spans(line: &TranscriptLine, wide: bool) -> Vec<Span<'static>> {
    let dim = Style::default().fg(theme::DIM);
    let mut spans = match line.gutter {
        Gutter::User => return vec![Span::raw("     ")],
        Gutter::None | Gutter::Reply | Gutter::ThreadHead => return Vec::new(),
        Gutter::Verify | Gutter::VerifyFail | Gutter::VerifySkip if wide => {
            vec![Span::styled("┆ ┆ ┆  ", dim)]
        }
        _ if wide => vec![Span::styled("│ │ │  ", dim)],
        _ => vec![Span::styled("   ", dim)],
    };
    if line.md_kind == Some(markdown::MdKind::Bullet)
        && let Some(marker) = line.spans.as_ref().and_then(|s| s.first())
    {
        spans.push(Span::raw(
            " ".repeat(UnicodeWidthStr::width(marker.0.as_str())),
        ));
    }
    spans
}

/// Renders `line` and splits it into rows no wider than `width`, word by
/// word, starting every row after the first with `continuation_spans`. Prose
/// (paragraph, bullet, heading, streaming text) is additionally capped at
/// `PROSE_MAX_COLS` text columns. Words longer than a row are cut on
/// character boundaries. Ratatui's own wrap would start continuation rows at
/// column 0 and lose the gutter.
pub(crate) fn wrap_transcript_line(
    line: &TranscriptLine,
    rendered: Line<'static>,
    width: u16,
) -> Vec<Line<'static>> {
    let wide = is_wide(width.saturating_add(1));
    let width = usize::from(width.max(1));
    let base_w = if wide { 7 } else { 3 };
    let mut limit = width;
    if line.gutter == Gutter::Prose
        && !matches!(
            line.md_kind,
            Some(markdown::MdKind::Code | markdown::MdKind::CodeFence)
        )
    {
        limit = limit.min(base_w + PROSE_MAX_COLS);
    }
    let has_newline = rendered.spans.iter().any(|s| s.content.contains('\n'));
    if !has_newline && spans_width(&rendered.spans) <= limit {
        return vec![rendered];
    }
    let cont = continuation_spans(line, wide);
    let cont_w = spans_width(&cont);
    let mut rows: Vec<Vec<Span<'static>>> = vec![Vec::new()];
    let mut cur_w = 0usize;
    let mut fresh = false;
    for span in &rendered.spans {
        let style = span.style;
        let mut pieces: Vec<(String, bool)> = Vec::new();
        for ch in span.content.chars() {
            let ws = ch.is_whitespace();
            match pieces.last_mut() {
                Some((p, w)) if *w == ws && ch != '\n' && p != "\n" => p.push(ch),
                _ => pieces.push((ch.to_string(), ws)),
            }
        }
        for (piece, ws) in pieces {
            if piece == "\n" {
                rows.push(cont.clone());
                cur_w = cont_w;
                fresh = true;
                continue;
            }
            let pw = UnicodeWidthStr::width(piece.as_str());
            if ws {
                if !fresh && cur_w + pw <= limit {
                    rows.last_mut().unwrap().push(Span::styled(piece, style));
                    cur_w += pw;
                }
                continue;
            }
            // An empty first row (no gutter, overlong first word) is filled
            // by the char split below instead of being left blank.
            if cur_w + pw > limit && !fresh && cur_w > 0 {
                rows.push(cont.clone());
                cur_w = cont_w;
            }
            if cur_w + pw <= limit {
                rows.last_mut().unwrap().push(Span::styled(piece, style));
                cur_w += pw;
                fresh = false;
                continue;
            }
            let mut chunk = String::new();
            for ch in piece.chars() {
                let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
                if cur_w + cw > limit && !chunk.is_empty() {
                    rows.last_mut()
                        .unwrap()
                        .push(Span::styled(std::mem::take(&mut chunk), style));
                    rows.push(cont.clone());
                    cur_w = cont_w;
                }
                chunk.push(ch);
                cur_w += cw;
            }
            if !chunk.is_empty() {
                rows.last_mut().unwrap().push(Span::styled(chunk, style));
            }
            fresh = false;
        }
    }
    rows.into_iter().map(Line::from).collect()
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

#[derive(Default)]
pub(crate) struct TranscriptCache {
    width: u16,
    epoch: u64,
    lines: Vec<CachedTranscriptLine>,
    /// Indices whose last render was inside the animation window (not settled).
    animating: Vec<usize>,
    /// Number of `render_line` calls so far (test-only instrumentation).
    #[cfg(test)]
    pub(crate) render_count: usize,
}

/// Motion inputs for one transcript render pass (see `TranscriptCache::update`).
#[derive(Clone, Copy)]
pub(crate) struct MotionCtx {
    pub now: Instant,
    pub reduced_motion: bool,
    /// `AppState::style_epoch`; a change invalidates every cached line.
    pub epoch: u64,
    /// Transcript index from which fact lines render bold (trace-back), or
    /// `None` when the trace-back is off.
    pub trace_from: Option<usize>,
}

struct CachedTranscriptLine {
    source: TranscriptLine,
    /// False while the line was rendered inside its animation window; such an
    /// entry is re-rendered once more after the window closes.
    settled: bool,
    entries: Vec<(Line<'static>, Option<usize>, [u16; 2])>,
}

impl TranscriptCache {
    /// Cached source of the line at `index` (tests).
    #[cfg(test)]
    pub(crate) fn probe_source(&self, index: usize) -> Option<&TranscriptLine> {
        self.lines.get(index).map(|cached| &cached.source)
    }

    /// First rendered row and settled flag of the cached line at `index`.
    #[cfg(test)]
    pub(crate) fn probe(&self, index: usize) -> Option<(&Line<'static>, bool)> {
        let cached = self.lines.get(index)?;
        Some((&cached.entries.first()?.0, cached.settled))
    }

    /// Incrementally refreshes the cache. `dirty_from` is the lowest index of
    /// an in-place edited transcript line (`AppState::transcript_dirty_from`,
    /// `usize::MAX` when none); appended lines are detected by length and
    /// still-animating lines are re-rendered every pass. No other line is
    /// scanned or compared.
    pub(crate) fn update(
        &mut self,
        transcript: &[TranscriptLine],
        width: u16,
        ctx: MotionCtx,
        dirty_from: usize,
    ) {
        if self.width != width || self.epoch != ctx.epoch || transcript.len() < self.lines.len() {
            self.lines.clear();
            self.animating.clear();
            self.width = width;
            self.epoch = ctx.epoch;
        }
        let start = dirty_from.min(self.lines.len());
        let mut todo: Vec<usize> = (start..transcript.len()).collect();
        todo.extend(
            self.animating
                .iter()
                .copied()
                .filter(|&i| i < start && i < transcript.len()),
        );
        let mut still_animating = Vec::new();
        for index in todo {
            if self.render_line(transcript, index, width, ctx) {
                still_animating.push(index);
            }
        }
        self.animating = still_animating;
        #[cfg(debug_assertions)]
        for (i, cached) in self.lines.iter().enumerate() {
            if cached.settled {
                assert!(
                    cached.source == transcript[i],
                    "transcript line {i} mutated in place without touch_transcript"
                );
            }
        }
    }

    /// Renders `transcript[index]` into the cache; returns whether it was
    /// still inside its animation window.
    fn render_line(
        &mut self,
        transcript: &[TranscriptLine],
        index: usize,
        width: u16,
        ctx: MotionCtx,
    ) -> bool {
        #[cfg(test)]
        {
            self.render_count += 1;
        }
        let source = &transcript[index];
        let animating = line_animating(source.born, ctx.now, ctx.reduced_motion);
        let header = !source.tool_children.is_empty();
        let trace_bold = ctx.trace_from.is_some_and(|start| index >= start);
        let wrap_width = width.saturating_sub(1);
        let main = transcript_line_at(source, width, ctx.now, ctx.reduced_motion, trace_bold);
        let mut entries: Vec<(Line<'static>, Option<usize>)> =
            wrap_transcript_line(source, main, wrap_width)
                .into_iter()
                .enumerate()
                .map(|(i, row)| {
                    (
                        row,
                        if i == 0 {
                            header.then_some(index)
                        } else {
                            None
                        },
                    )
                })
                .collect();
        if header && source.expanded {
            for child in &source.tool_children {
                let child_line = TranscriptLine::new(Gutter::Tool, format!("  {child}"));
                let rendered = transcript_line_to_ratatui(&child_line, width);
                entries.extend(
                    wrap_transcript_line(&child_line, rendered, wrap_width)
                        .into_iter()
                        .map(|row| (row, None)),
                );
            }
        }
        let cached = CachedTranscriptLine {
            source: source.clone(),
            settled: !animating,
            entries: entries
                .into_iter()
                .map(|(line, index)| {
                    let rows = [
                        line_rows(&line, width),
                        line_rows(&line, width.saturating_sub(1)),
                    ];
                    (line, index, rows)
                })
                .collect(),
        };
        if index < self.lines.len() {
            self.lines[index] = cached;
        } else {
            self.lines.push(cached);
        }
        animating
    }
}

/// Keep ratatui's exact wrapping, but skip logical lines above the viewport.
pub(crate) fn visible_transcript_lines(
    lines: &[&Line<'static>],
    rows: &[u16],
    scroll: u16,
    height: u16,
) -> (Vec<Line<'static>>, u16) {
    let mut start = 0usize;
    let scroll = usize::from(scroll);
    let end = scroll + usize::from(height);
    let mut offset = 0;
    let mut visible = Vec::new();
    for (line, count) in lines.iter().zip(rows) {
        let next = start + usize::from(*count);
        if next > scroll && start < end {
            if visible.is_empty() {
                offset = scroll.saturating_sub(start) as u16;
            }
            visible.push((*line).clone());
        }
        start = next;
        if start >= end {
            break;
        }
    }
    (visible, offset)
}

pub(crate) const MAX_INPUT_LINES: usize = 6;
pub(crate) const MAX_VISIBLE_PASTE_ATTACHMENTS: usize = 2;

pub(crate) fn composer_height(state: &AppState) -> u16 {
    let total = state.pasted_attachments.len() + state.image_attachments.len();
    let attachment_rows = total.min(MAX_VISIBLE_PASTE_ATTACHMENTS) + usize::from(total > 2);
    let rows = input_visual_rows(&state.input, state.input_columns).len();
    (rows.clamp(1, MAX_INPUT_LINES) as u16 + 2).saturating_add(attachment_rows as u16)
}

/// Builds the step rail (header row 2): the run's steps with a done `✓`, the
/// active spinner, or a dim future `·`, plus right-aligned elapsed `m:ss`
/// (live while running, the receipt's total once finished). With no run and
/// no receipt every step reads as future. Narrow widths collapse to
/// ` 3/4 CHANGE ◐`.
pub(crate) fn step_rail_line(state: &AppState, width: u16) -> Line<'static> {
    let w = width as usize;
    let steps = &state.ledger.steps;
    let has_progress = state.running || state.completion.is_some();
    let done_at = |i: usize| has_progress && steps.get(i).is_some_and(|(_, done)| *done);
    let active = if state.running {
        steps.iter().position(|(_, done)| !*done)
    } else {
        None
    };
    let elapsed_ms = if state.running {
        state.run_started.map(|t| t.elapsed().as_millis())
    } else {
        state.completion.as_ref().map(|c| c.elapsed_ms)
    };
    let streaming = !state.current_stream.is_empty() || !state.stream_pending.is_empty();
    let spin = spinner_glyph(
        state.run_started.map_or(0, |t| t.elapsed().as_millis()),
        state.reduced_motion,
        streaming,
    );
    let name_style = Style::default();
    let mut spans = vec![Span::raw(" ")];
    if is_wide(width) {
        for (i, (step, _)) in steps.iter().enumerate() {
            let label = task_step_label(*step);
            if done_at(i) {
                spans.push(Span::styled(label, name_style));
                spans.push(Span::styled(
                    " ✓",
                    Style::default().fg(theme::OK).add_modifier(Modifier::BOLD),
                ));
            } else if active == Some(i) {
                spans.push(Span::styled(
                    label,
                    Style::default().add_modifier(Modifier::BOLD),
                ));
                spans.push(Span::styled(
                    format!(" {spin}"),
                    Style::default().fg(theme::T),
                ));
            } else {
                spans.push(Span::styled(label, Style::default().fg(theme::MUTED)));
                spans.push(Span::styled(" ·", Style::default().fg(theme::DIM)));
            }
            if i + 1 < steps.len() {
                if done_at(i) {
                    let since = state
                        .ledger
                        .done_at
                        .iter()
                        .find(|(s, _)| s == step)
                        .map_or(u128::MAX, |(_, t)| t.elapsed().as_millis());
                    if rail_fill_cells(since, state.reduced_motion) == 1 {
                        spans.push(Span::raw(" ━"));
                        spans.push(Span::styled("─", Style::default().fg(theme::DIM)));
                        spans.push(Span::raw(" "));
                    } else {
                        spans.push(Span::raw(" ━━ "));
                    }
                } else {
                    spans.push(Span::styled(" ── ", Style::default().fg(theme::DIM)));
                }
            }
        }
    } else {
        let current = active.or_else(|| (0..steps.len()).rev().find(|i| done_at(*i)));
        let (index, label, glyph) = match current {
            Some(i) if active == Some(i) => (i + 1, task_step_label(steps[i].0), spin.to_string()),
            Some(i) => (i + 1, task_step_label(steps[i].0), "✓".to_string()),
            None => (
                0,
                steps.first().map_or("", |(s, _)| task_step_label(*s)),
                "·".to_string(),
            ),
        };
        spans.push(Span::raw(format!(
            "{index}/{} {label} {glyph}",
            steps.len()
        )));
    }
    if let Some(ms) = elapsed_ms {
        let text = fmt_mss(ms);
        let used = spans_width(&spans);
        let text_w = text.chars().count();
        if used + text_w + 2 <= w {
            let pad = w - used - text_w - 1;
            let cells = state.pulse.len().min(pad.saturating_sub(4));
            if cells >= 4 {
                spans.push(Span::raw(" ".repeat(pad - cells - 2)));
                spans.extend(pulse_spans(&state.pulse, cells));
                spans.push(Span::raw("  "));
            } else {
                spans.push(Span::raw(" ".repeat(pad)));
            }
            spans.push(Span::styled(text, Style::default().fg(theme::MUTED)));
        }
    }
    Line::from(spans)
}

fn context_receipt_line(state: &AppState) -> Option<Line<'static>> {
    let knowledge = state.knowledge.as_ref()?;
    let total = knowledge.zindeks.len() + knowledge.ingat.len() + knowledge.git.len();
    if total == 0 {
        return None;
    }
    Some(Line::from(vec![
        Span::styled(
            if state.knowledge_band_open {
                " ▾ CONTEXT  "
            } else {
                " ▸ CONTEXT  "
            },
            Style::default()
                .fg(theme::MUTED)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("{} code facts", knowledge.zindeks.len()),
            Style::default().fg(theme::Z),
        ),
        Span::raw(" · "),
        Span::styled(
            format!("{} memories", knowledge.ingat.len()),
            Style::default().fg(theme::I),
        ),
        Span::raw(" · "),
        Span::styled(
            format!("{} git facts", knowledge.git.len()),
            Style::default().fg(theme::G),
        ),
        Span::styled(
            format!(
                " · {}/{} tokens",
                compact_tokens(knowledge.context_tokens),
                compact_tokens(knowledge.budget_tokens)
            ),
            Style::default().fg(theme::DIM),
        ),
        Span::styled("   Ctrl+K", Style::default().fg(theme::DIM)),
    ]))
}

pub(crate) fn focus_surface_lines(state: &AppState) -> Vec<Line<'static>> {
    let bold = Style::default().add_modifier(Modifier::BOLD);
    if let Some(permission) = state.pending.front() {
        let mut lines = vec![Line::from(Span::styled(
            " PERMISSION",
            Style::default()
                .fg(theme::WARN)
                .add_modifier(Modifier::BOLD),
        ))];
        for line in permission.summary.lines() {
            lines.push(Line::from(Span::styled(format!(" {line}"), bold)));
        }
        lines.push(Line::from(vec![
            Span::styled(" Scope  ", Style::default().fg(theme::MUTED)),
            Span::raw("this invocation only"),
        ]));
        lines.push(Line::from(vec![
            Span::styled(" [A]", bold),
            Span::raw(" Allow once   "),
            Span::styled("[D]", bold),
            Span::raw(" Deny"),
        ]));
        return lines;
    }

    if state.running {
        // The step rail and now-line already cover a running task; only the
        // opt-in evidence band (Ctrl+K) adds rows here.
        let mut lines = Vec::new();
        if state.knowledge_band_open {
            lines.extend(context_receipt_line(state));
            if let Some(knowledge) = &state.knowledge {
                lines.extend(knowledge_band_lines(
                    knowledge,
                    state.render_tick,
                    state.reduced_motion,
                ));
            }
        }
        return lines;
    }

    if let Some(error) = &state.last_error {
        let mut lines = vec![
            Line::from(Span::styled(
                " FAILED",
                Style::default().fg(theme::ERR).add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::raw(format!(" {}", truncate_chars(error, 180)))),
        ];
        for row in state.ledger.numstat.iter().take(2) {
            lines.push(Line::from(vec![
                Span::styled(" GIT    ", Style::default().fg(theme::G)),
                Span::raw(row.path.clone()),
                Span::styled(
                    format!("  +{} -{}", row.added, row.deleted),
                    Style::default().fg(theme::MUTED),
                ),
            ]));
        }
        if !state.ledger.numstat.is_empty() {
            lines.push(Line::from(Span::styled(
                " still in the working tree, unverified",
                Style::default().fg(theme::WARN),
            )));
        }
        lines.push(Line::from(Span::styled(
            " Edit the task and press Enter to try again.",
            Style::default().fg(theme::DIM),
        )));
        return lines;
    }

    if let Some(receipt) = &state.completion {
        const MAX_FILES: usize = 3;
        let mut lines = vec![Line::from(Span::styled(" RECEIPT", bold))];
        for row in receipt.numstat.iter().take(MAX_FILES) {
            lines.push(Line::from(vec![
                Span::raw(format!(" {} ", row.path)),
                Span::styled(format!("+{}", row.added), Style::default().fg(theme::OK)),
                Span::raw(" "),
                Span::styled(format!("−{}", row.deleted), Style::default().fg(theme::ERR)),
            ]));
        }
        if receipt.numstat.len() > MAX_FILES {
            lines.push(Line::from(Span::styled(
                format!(" +{} more", receipt.numstat.len() - MAX_FILES),
                Style::default().fg(theme::MUTED),
            )));
        }
        let mut checks = vec![Span::raw(" ")];
        if receipt.verify_steps.is_empty() {
            checks.push(Span::styled(
                "no checks ran",
                Style::default().fg(theme::WARN),
            ));
        }
        for (i, (name, status)) in receipt.verify_steps.iter().enumerate() {
            let (mark, color) = check_mark(*status);
            if i > 0 {
                checks.push(Span::raw("  "));
            }
            checks.push(Span::raw(format!("{name} ")));
            checks.push(Span::styled(mark, Style::default().fg(color)));
        }
        lines.push(Line::from(checks));
        lines.push(if receipt.input_tokens == 0 && receipt.output_tokens == 0 {
            Line::from(Span::styled(
                " tokens ? not reported",
                Style::default().fg(theme::WARN),
            ))
        } else {
            Line::from(Span::styled(
                format!(
                    " {} in ({}) · {} out",
                    compact_tokens(receipt.input_tokens as usize),
                    cached_label(receipt.cached_tokens),
                    compact_tokens(receipt.output_tokens as usize)
                ),
                Style::default().fg(theme::MUTED),
            ))
        });
        return lines;
    }

    let mut lines: Vec<Line<'static>> = context_receipt_line(state).into_iter().collect();
    if state.knowledge_band_open
        && let Some(knowledge) = &state.knowledge
    {
        lines.extend(knowledge_band_lines(
            knowledge,
            state.render_tick,
            state.reduced_motion,
        ));
    }
    lines
}

/// Builds the now-line: exactly one row above the composer input that says
/// what is happening, for how long, and what the next key does. Left side is
/// the first matching state; the right-aligned key hints drop when they do
/// not fit.
pub(crate) fn now_line(state: &AppState, width: u16) -> Line<'static> {
    let w = width as usize;
    let narrow = !is_wide(width);
    let reduced = state.reduced_motion;
    let run_ms = state.run_started.map_or(0, |t| t.elapsed().as_millis());
    let streaming = !state.current_stream.is_empty() || !state.stream_pending.is_empty();
    let spin = spinner_glyph(run_ms, reduced, false);
    let counter = fmt_counter(run_ms, reduced);
    let glyph_bold = |glyph: String, color: Color| {
        Span::styled(
            glyph,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        )
    };
    let mut left = vec![Span::raw(" ")];
    let mut right: Option<String> = None;
    if state.exit_confirmation_active() {
        left.push(Span::styled(
            "Ctrl+C or Ctrl+D again to exit · draft stays until then",
            Style::default().fg(theme::MUTED),
        ));
    } else if state.select_mode {
        left.push(Span::styled(
            "Ctrl+T exit select mode",
            Style::default().fg(theme::MUTED),
        ));
    } else if !state.pending.is_empty() {
        left.push(glyph_bold("⏸".to_string(), theme::WARN));
        left.push(Span::styled(
            " waiting for you",
            Style::default().fg(theme::WARN),
        ));
        right = Some("A allow once · D deny · Esc deny".to_string());
    } else if state.running {
        match state.status.state {
            RunState::Tool => {
                let name = state.current_tool.as_deref().unwrap_or("tool");
                let tool_ms = state
                    .tool_started
                    .map_or(run_ms, |t| t.elapsed().as_millis());
                left.push(Span::styled(
                    format!("{spin} "),
                    Style::default().fg(theme::T),
                ));
                left.push(Span::raw("tool: "));
                left.push(Span::styled(
                    truncate_chars(name, 36),
                    Style::default().add_modifier(Modifier::BOLD),
                ));
                left.push(Span::raw(format!(" · {}", fmt_counter(tool_ms, reduced))));
            }
            RunState::Verify => {
                left.push(Span::styled(
                    format!("{spin} "),
                    Style::default().fg(theme::T),
                ));
                left.push(Span::raw(format!("verifying · {counter}")));
            }
            _ if streaming => {
                left.push(Span::styled("▸ ", Style::default().fg(theme::T)));
                left.push(Span::raw(format!("model writing · {counter}")));
            }
            _ => {
                left.push(Span::styled(
                    format!("{spin} "),
                    Style::default().fg(theme::T),
                ));
                left.push(Span::raw(format!("model thinking · {counter}")));
            }
        }
        right = Some(
            if state.steering_active && narrow {
                "Enter steer · Esc cancel"
            } else if state.steering_active {
                "Enter steer · Alt+Enter queue · Esc cancel"
            } else {
                "Enter queue · Esc cancel"
            }
            .to_string(),
        );
    } else if state.graph_offer.is_some()
        && state.completion.is_some()
        && !state.composer_has_content()
    {
        left.push(glyph_bold("✓ ".to_string(), theme::OK));
        left.push(Span::styled(
            "done · graph answer",
            Style::default().fg(theme::OK),
        ));
        right = Some("Enter ask model anyway · Esc done".to_string());
    } else if state.memory_offer.is_some() && !state.composer_has_content() {
        left.push(glyph_bold("◇".to_string(), theme::I));
        left.push(Span::styled(" remember?", Style::default().fg(theme::I)));
        right = Some("Enter save · Tab team · Ctrl+E edit · Esc skip".to_string());
    } else if let Some(error) = &state.last_error {
        let prefix = "✗ stopped · ";
        let room = w.saturating_sub(1 + prefix.chars().count());
        let first = error.lines().next().unwrap_or("");
        left.push(glyph_bold("✗".to_string(), theme::ERR));
        left.push(Span::styled(" stopped", Style::default().fg(theme::ERR)));
        left.push(Span::raw(format!(" · {}", truncate_chars(first, room))));
        right = Some("Enter follow-up".to_string());
    } else if let Some(receipt) = &state.completion {
        let (text, color, glyph) = match receipt.verification_label() {
            "DONE · VERIFIED" => ("done · verified", theme::OK, true),
            "DONE · FAILED VERIFICATION" => ("done · failed verification", theme::ERR, false),
            _ => ("done · unverified", theme::WARN, false),
        };
        if glyph {
            left.push(glyph_bold("✓ ".to_string(), color));
        }
        left.push(Span::styled(text, Style::default().fg(color)));
        left.push(Span::styled(
            format!(" · {}", fmt_mss(receipt.elapsed_ms)),
            Style::default().fg(theme::MUTED),
        ));
        right = Some("Enter follow-up".to_string());
    } else {
        left.push(glyph_bold("●".to_string(), theme::OK));
        left.push(Span::raw(" ready"));
        right = Some(
            if !state.pasted_attachments.is_empty() || !state.image_attachments.is_empty() {
                "Enter send · Backspace remove last · Ctrl+A inspect"
            } else if narrow {
                "Enter send · / commands"
            } else {
                "Enter send · / commands · ? help"
            }
            .to_string(),
        );
    }
    if let Some(hint) = right {
        let used = spans_width(&left);
        let hint_w = hint.chars().count();
        if used + hint_w + 2 <= w {
            left.push(Span::raw(" ".repeat(w - used - hint_w - 1)));
            left.push(Span::styled(hint, Style::default().fg(theme::MUTED)));
        }
    }
    Line::from(left)
}

fn draw_focus_surface(
    f: &mut ratatui::Frame,
    area: ratatui::layout::Rect,
    lines: Vec<Line<'static>>,
) {
    if area.height == 0 {
        return;
    }
    let mut rendered = vec![Line::from(Span::styled(
        "─".repeat(area.width as usize),
        Style::default().fg(theme::DIM),
    ))];
    rendered.extend(
        lines
            .into_iter()
            .take(area.height.saturating_sub(1) as usize),
    );
    f.render_widget(Paragraph::new(rendered).wrap(Wrap { trim: false }), area);
}

fn centered_overlay(area: ratatui::layout::Rect, width: u16, height: u16) -> ratatui::layout::Rect {
    let width = width.min(area.width.saturating_sub(2)).max(1);
    let height = height.min(area.height.saturating_sub(2)).max(1);
    ratatui::layout::Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    }
}

pub(crate) fn shortcut_sheet_lines(state: &AppState) -> Vec<Line<'static>> {
    let enter = match state.composer_mode() {
        ComposerMode::Ask => "send task",
        ComposerMode::Steer => "steer active run",
        ComposerMode::Queue => "queue next task",
        ComposerMode::Decision => "choose permission action",
        ComposerMode::Recover => "retry with a revised task",
        ComposerMode::FollowUp => "review diff or send follow-up",
    };
    vec![
        Line::from(Span::styled(
            " SHORTCUTS",
            Style::default()
                .fg(theme::MUTED)
                .add_modifier(Modifier::BOLD),
        )),
        Line::default(),
        Line::from(format!(" Enter        {enter}")),
        Line::from(" Alt+Enter    queue during an active run"),
        Line::from(" Shift+Enter  newline"),
        Line::from(" Arrows       edit prompt; Up/Down recall history"),
        Line::from(" /            actions and commands"),
        Line::from(" Ctrl+P       command palette (keeps draft)"),
        Line::from(" Ctrl+C/D     repeat within 2s to exit when idle"),
        Line::from(" Ctrl+K       expand evidence"),
        Line::from(" Ctrl+L       open Run Map"),
        Line::from(" Ctrl+A       inspect attachments"),
        Line::from(" Ctrl+Y       copy last response"),
        Line::from(" Ctrl+T       terminal select mode"),
        Line::from(" Shift+Tab    toggle auto mode"),
        Line::from(" Esc          cancel active run"),
        Line::default(),
        Line::from(Span::styled(
            " ? or Esc closes this sheet",
            Style::default().fg(theme::DIM),
        )),
    ]
}

pub(crate) fn attachment_inspector_lines(state: &AppState) -> Vec<Line<'static>> {
    let mut lines = vec![
        Line::from(Span::styled(
            " ATTACHMENTS",
            Style::default()
                .fg(theme::MUTED)
                .add_modifier(Modifier::BOLD),
        )),
        Line::default(),
    ];
    for (kind, summary) in state.attachment_rows() {
        let label = match kind {
            AttachmentKind::Text => " TXT  ",
            AttachmentKind::Image => " IMG  ",
        };
        lines.push(Line::from(vec![
            Span::styled(label, Style::default().fg(theme::Z)),
            Span::raw(summary),
        ]));
    }
    if state.pasted_attachments.is_empty() && state.image_attachments.is_empty() {
        lines.push(Line::from(Span::styled(
            " No attachments in the composer.",
            Style::default().fg(theme::DIM),
        )));
    } else {
        lines.push(Line::default());
        lines.push(Line::from(Span::styled(
            " Backspace on empty input removes the latest · Ctrl+A or Esc closes",
            Style::default().fg(theme::DIM),
        )));
    }
    lines
}

fn draw_sheet(f: &mut ratatui::Frame, lines: Vec<Line<'static>>) {
    let height = (lines.len() as u16 + 2).min(f.area().height.saturating_sub(2));
    let popup = centered_overlay(f.area(), 72, height);
    f.render_widget(Clear, popup);
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), popup);
}

/// Fits `/why` into the screen: keeps the header and the "Esc closes"
/// footer, and marks how many middle rows were cut.
pub(crate) fn why_sheet_lines(lines: &[String], max_rows: usize) -> Vec<Line<'static>> {
    let styled = |s: &str| Line::from(s.to_string());
    if lines.len() <= max_rows || max_rows < 4 {
        return lines.iter().map(|l| styled(l)).collect();
    }
    let keep_top = max_rows - 2;
    let mut out: Vec<Line<'static>> = lines[..keep_top].iter().map(|l| styled(l)).collect();
    out.push(styled(&format!(
        " … {} more rows (enlarge the terminal)",
        lines.len() - keep_top - 1
    )));
    out.push(styled(lines.last().unwrap()));
    out
}

pub(crate) fn draw(f: &mut ratatui::Frame, state: &mut AppState, cwd: &Path) {
    state.input_columns = f.area().width.saturating_sub(4).max(1) as usize;
    let mut focus_lines = if state.ledger_open {
        Vec::new()
    } else {
        focus_surface_lines(state)
    };
    if !state.pending.is_empty() {
        focus_lines.insert(
            2,
            Line::from(vec![
                Span::styled(" cwd    ", Style::default().fg(theme::MUTED)),
                Span::raw(truncate_chars(
                    &cwd.display().to_string(),
                    f.area().width.saturating_sub(8) as usize,
                )),
            ]),
        );
    }
    let hint_items = if !state.picker.open
        && state.pending.is_empty()
        && !state.ledger_open
        && state.pasted_attachments.is_empty()
        && state.image_attachments.is_empty()
        && state.input.starts_with('/')
    {
        let custom = state.custom_commands(cwd);
        slash_hint_items(&state.input, &custom)
    } else {
        Vec::new()
    };
    // Reserve the transcript and composer before assigning optional rows.
    // At 60×20 this prevents slash hints or a long receipt from hiding input.
    let optional = f.area().height.saturating_sub(6);
    let composer_rows = if state.pending.is_empty() {
        composer_height(state)
    } else {
        3
    }
    .min(optional);
    let remaining = optional.saturating_sub(composer_rows);
    let focus_height = if !state.pending.is_empty() {
        // ponytail: nine rows fit normal command summaries at 60×20; add scrolling if longer approvals appear.
        9
    } else if focus_lines.is_empty() {
        0
    } else {
        (focus_lines.len() as u16 + 1).min(if f.area().height <= 20 { 6 } else { 9 })
    }
    .min(remaining);
    let hint_height = (hint_items.len() as u16).min(remaining.saturating_sub(focus_height));

    let mut constraints = vec![Constraint::Length(3), Constraint::Min(3)];
    if !focus_lines.is_empty() {
        constraints.push(Constraint::Length(focus_height));
    }
    if !hint_items.is_empty() {
        constraints.push(Constraint::Length(hint_height));
    }
    constraints.push(Constraint::Length(composer_rows));

    let areas = Layout::vertical(constraints).split(f.area());
    let mut idx = 0;

    f.render_widget(
        Paragraph::new(vec![
            scope_line(state, areas[idx].width),
            step_rail_line(state, areas[idx].width),
            Line::from(Span::styled(
                "─".repeat(areas[idx].width as usize),
                Style::default().fg(theme::DIM),
            )),
        ]),
        areas[idx],
    );
    idx += 1;

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
        // the now-line) is `None`.
        let transcript_area = areas[idx];
        state.transcript_cache.update(
            &state.transcript,
            transcript_area.width,
            MotionCtx {
                now: Instant::now(),
                reduced_motion: state.reduced_motion,
                epoch: state.style_epoch,
                trace_from: state.trace_back.map(|_| state.run_transcript_start),
            },
            state.transcript_dirty_from,
        );
        state.transcript_dirty_from = usize::MAX;
        let mut extra = Vec::new();
        if show_empty_state(&state.transcript, state.running) {
            extra.extend(empty_state_lines(state, transcript_area.width));
        }
        let empty_len = extra.len();
        if !state.current_stream.is_empty() {
            for part in state.current_stream.split('\n') {
                if part.is_empty() {
                    extra.push(Line::default());
                    continue;
                }
                let tl = TranscriptLine::new(Gutter::Prose, part);
                let rendered = transcript_line_to_ratatui(&tl, transcript_area.width);
                extra.extend(wrap_transcript_line(
                    &tl,
                    rendered,
                    transcript_area.width.saturating_sub(1),
                ));
            }
        }
        let mut text_lines = Vec::new();
        let mut indices = Vec::new();
        let mut full_rows = Vec::new();
        let mut narrow_rows = Vec::new();
        let measured_extra: Vec<_> = extra
            .iter()
            .map(|line| {
                (
                    line,
                    None,
                    [
                        line_rows(line, transcript_area.width),
                        line_rows(line, transcript_area.width.saturating_sub(1)),
                    ],
                )
            })
            .collect();
        let cached = state
            .transcript_cache
            .lines
            .iter()
            .flat_map(|cached| &cached.entries)
            .map(|(line, index, rows)| (line, *index, *rows));
        for (line, index, rows) in measured_extra[..empty_len]
            .iter()
            .copied()
            .chain(cached)
            .chain(measured_extra[empty_len..].iter().copied())
        {
            text_lines.push(line);
            indices.push(index);
            full_rows.push(rows[0]);
            narrow_rows.push(rows[1]);
        }

        // Decide, at the full transcript width, whether a scrollbar column
        // needs reserving. If it does, the text area narrows by one column
        // — re-measure at that narrower width so wrap/clamp/scrollbar/hit-
        // test math all agree with what's actually rendered (narrowing can
        // only add wrapped lines, never remove the overflow, so this never
        // flaps).
        let total_lines_full = lines_as_u16(full_rows.iter().map(|n| usize::from(*n)).sum());
        let scrollbar_needed = total_lines_full > transcript_area.height;
        let (text_area, scrollbar_area) = if scrollbar_needed && transcript_area.width > 1 {
            let cols = Layout::horizontal([Constraint::Min(1), Constraint::Length(1)])
                .split(transcript_area);
            (cols[0], Some(cols[1]))
        } else {
            (transcript_area, None)
        };
        let row_counts = if scrollbar_area.is_some() {
            narrow_rows
        } else {
            full_rows
        };
        let total_lines = if scrollbar_area.is_some() {
            lines_as_u16(row_counts.iter().map(|n| usize::from(*n)).sum())
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
        let rows: Vec<(u16, Option<usize>)> = row_counts
            .iter()
            .zip(indices.iter())
            .map(|(count, idx)| (*count, *idx))
            .collect();
        state.transcript_hit = Some(TranscriptHit {
            area: text_area,
            scroll: state.scroll,
            rows,
        });

        let (visible, offset) =
            visible_transcript_lines(&text_lines, &row_counts, state.scroll, viewport_height);
        let transcript = Paragraph::new(visible)
            .wrap(Wrap { trim: false })
            .block(Block::default().borders(Borders::NONE))
            .scroll((offset, 0));
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

    if !focus_lines.is_empty() {
        draw_focus_surface(f, areas[idx], focus_lines);
        idx += 1;
    }

    let input_area = if hint_items.is_empty() {
        areas[idx]
    } else {
        f.render_widget(
            Paragraph::new({
                let count = areas[idx].height as usize;
                let start = state.slash_selected.saturating_add(1).saturating_sub(count);
                slash_hint_lines(
                    &hint_items[start..(start + count).min(hint_items.len())],
                    state.slash_selected.saturating_sub(start),
                )
            }),
            areas[idx],
        );
        idx += 1;
        areas[idx]
    };

    draw_input(f, input_area, state);

    if let Some(lines) = &state.why_lines {
        let max_rows = f.area().height.saturating_sub(4) as usize;
        draw_sheet(f, why_sheet_lines(lines, max_rows));
    } else if state.picker.open {
        draw_picker(f, &state.picker);
    } else if state.shortcuts_open {
        draw_sheet(f, shortcut_sheet_lines(state));
    } else if state.attachments_open {
        draw_sheet(f, attachment_inspector_lines(state));
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

/// Renders the input form: a DIM full-width rule, then up to six editable
/// lines. `Shift+Enter` inserts a newline; overflow keeps the newest lines
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
    let composer_area = ratatui::layout::Rect {
        y: area.y + 1,
        height: area.height - 1,
        ..area
    };

    let mut rendered = vec![now_line(state, area.width)];
    let attachment_summaries: Vec<(&str, String)> = state
        .attachment_rows()
        .into_iter()
        .map(|(kind, summary)| {
            (
                match kind {
                    AttachmentKind::Text => "TXT",
                    AttachmentKind::Image => "IMG",
                },
                summary,
            )
        })
        .collect();
    let attachment_count = attachment_summaries
        .len()
        .min(MAX_VISIBLE_PASTE_ATTACHMENTS)
        .min(composer_area.height.saturating_sub(2) as usize);
    let attachment_start = attachment_summaries.len().saturating_sub(attachment_count);
    for (kind, summary) in &attachment_summaries[attachment_start..] {
        rendered.push(Line::from(vec![
            Span::styled(
                format!(" {kind:<4}"),
                Style::default().fg(theme::Z).add_modifier(Modifier::BOLD),
            ),
            Span::styled(summary, Style::default().fg(theme::MUTED)),
        ]));
    }
    let overflow = attachment_summaries.len().saturating_sub(attachment_count);
    if overflow > 0 {
        rendered.push(Line::from(Span::styled(
            format!("      +{overflow} more · Ctrl+A inspect all"),
            Style::default().fg(theme::DIM),
        )));
    }

    let input_area = ratatui::layout::Rect {
        y: composer_area.y + rendered.len() as u16,
        height: composer_area.height.saturating_sub(rendered.len() as u16),
        ..composer_area
    };
    if input_area.height == 0 {
        f.render_widget(Paragraph::new(rendered), composer_area);
        return;
    }

    let rows = input_visual_rows(&state.input, input_area.width.saturating_sub(4) as usize);
    let cursor_byte = state.cursor_byte();
    let cursor_row = rows
        .iter()
        .rposition(|row| row.start <= cursor_byte)
        .unwrap_or(0);
    let visible_count = rows.len().min(input_area.height as usize);
    let visible_start = (cursor_row + 1).saturating_sub(visible_count);
    for (visible_index, row) in rows[visible_start..visible_start + visible_count]
        .iter()
        .enumerate()
    {
        let actual_index = visible_start + visible_index;
        let prefix = if actual_index == 0 { " › " } else { " │ " };
        let spans = vec![
            Span::styled(prefix, Style::default().fg(theme::MUTED)),
            Span::raw(state.input[row.start..row.end].to_string()),
        ];
        rendered.push(Line::from(spans));
    }

    f.render_widget(Paragraph::new(rendered), composer_area);

    if !state.picker.open
        && !state.shortcuts_open
        && !state.attachments_open
        && state.pending.is_empty()
    {
        let row = rows[cursor_row];
        let column = UnicodeWidthStr::width(&state.input[row.start..cursor_byte.min(row.end)]);
        let cursor_x = (input_area.x as usize + 3 + column)
            .min((input_area.x + input_area.width).saturating_sub(1) as usize)
            as u16;
        let cursor_y = input_area.y + (cursor_row - visible_start) as u16;
        f.set_cursor_position((cursor_x, cursor_y));
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

/// Renders the Run Map view (Ctrl+L): OBJECTIVE and real task progress.
/// (`✓` done / `●`/`◉` active pulse / `○` pending — no borders, DIM rule
/// spacing only), CURRENT CHANGE, and WHY. Every row traces to a real
/// event; no invented captions. `running`/`elapsed_ms`/`reduced_motion`
/// drive the active marker's 1 Hz pulse.
pub(crate) fn draw_ledger(
    f: &mut ratatui::Frame,
    area: ratatui::layout::Rect,
    ledger: &LedgerState,
    running: bool,
    elapsed_ms: u128,
    reduced_motion: bool,
) {
    let mut lines: Vec<Line<'static>> = Vec::new();

    lines.push(Line::from(Span::styled(
        " RUN MAP",
        Style::default()
            .fg(theme::MUTED)
            .add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::default());

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
                    .map(|(name, status)| format!("{name} {}", check_mark(*status).0))
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
            // Diff isn't a graph/memory/git provenance source, so per DESIGN.md ("color
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

/// Centered overlay listing catalog entries. The visible window follows the
/// highlighted row so keyboard selection never disappears below the viewport.
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
        PickerKind::Command => "commands",
        PickerKind::Setup => "setup",
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
        "type to filter · Enter select · Esc cancel",
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
            if picker.kind == PickerKind::Model {
                "(no matches · Esc, then /model <name> for custom)"
            } else {
                "(no matches)"
            },
            Style::default().fg(theme::DIM),
        )));
    }
    let capacity = (popup.height as usize).saturating_sub(lines.len());
    let start = picker.selected.saturating_add(1).saturating_sub(capacity);
    for (i, item) in filtered.iter().enumerate().skip(start).take(capacity) {
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
