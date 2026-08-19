use std::time::{Duration, Instant};

use kode_core::event::{KodeEvent, NoteSource, TaskStep};

use super::draw::should_flush_stream_buffer;
use super::markdown;
use super::state::*;

/// Applies one `KodeEvent` to `state`. Any accumulated `current_stream` text
/// is flushed into the transcript before non-token events are processed, so
/// the transcript always reads as a sequence of complete lines.
pub fn apply_event(state: &mut AppState, ev: KodeEvent) {
    // Any non-token event ends the current streaming window: drain
    // whatever's still buffered (not yet flushed to the visible stream)
    // into `current_stream` unconditionally, so no trailing text is lost
    // when a tool call/finish/etc. interrupts mid-word.
    if !matches!(ev, KodeEvent::ModelToken { .. }) {
        if !state.stream_pending.is_empty() {
            let pending = std::mem::take(&mut state.stream_pending);
            state.current_stream.push_str(&pending);
        }
        state.stream_last_flush = None;
    }

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
            // Coalesce: buffer deltas and only flush into the visible
            // `current_stream` on a word/whitespace boundary or once the
            // 120ms window elapses — avoids a character-typewriter effect
            // and keeps the transcript the single moving region while a
            // stream is producing (spinner freezes — see `spinner_glyph`).
            if state.stream_pending.is_empty() {
                state.stream_last_flush = Some(Instant::now());
            }
            state.stream_pending.push_str(&text);
            if let Some(ap) = &mut state.aperture {
                ap.trigger_seen = true;
            }
            let elapsed = state
                .stream_last_flush
                .map(|t| t.elapsed())
                .unwrap_or(Duration::ZERO);
            if should_flush_stream_buffer(&state.stream_pending, elapsed) {
                let pending = std::mem::take(&mut state.stream_pending);
                state.current_stream.push_str(&pending);
                state.stream_last_flush = None;
            }
        }
        KodeEvent::ToolRequested { .. } => {}
        KodeEvent::ToolStarted { name } => {
            // Stack consecutive tool calls into one collapsible group header
            // instead of flooding the transcript with a line per call (e.g.
            // many `read_file` lines in a row) — see `tool_group_summary`.
            // Only Tool lines directly adjacent group; any other line
            // (prose, a note, a failure) breaks the run and starts a new
            // header on the next ToolStarted.
            match state.transcript.last_mut() {
                Some(last) if last.gutter == Gutter::Tool => {
                    if last.tool_children.is_empty() {
                        let prior = std::mem::take(&mut last.text);
                        last.tool_children.push(prior);
                    }
                    last.tool_children.push(name.clone());
                    last.text = tool_group_summary(&last.tool_children);
                }
                _ => {
                    state
                        .transcript
                        .push(TranscriptLine::new(Gutter::Tool, name.clone()));
                }
            }
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
        KodeEvent::SourcedNote { text, source } => {
            let gutter = match source {
                NoteSource::Zindeks => Gutter::Zindeks,
                NoteSource::Ingat => Gutter::Ingat,
                NoteSource::Git => Gutter::Git,
            };
            state.transcript.push(TranscriptLine::new(gutter, text));
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
            let prev = state.knowledge.as_ref();
            let zindeks_since_tick = match zindeks.first() {
                Some(f)
                    if prev.and_then(|k| k.zindeks.first()).map(String::as_str)
                        == Some(f.as_str()) =>
                {
                    prev.and_then(|k| k.zindeks_since_tick)
                }
                Some(_) => Some(state.render_tick),
                None => None,
            };
            let ingat_since_tick = match ingat.first() {
                Some(f)
                    if prev.and_then(|k| k.ingat.first()).map(String::as_str)
                        == Some(f.as_str()) =>
                {
                    prev.and_then(|k| k.ingat_since_tick)
                }
                Some(_) => Some(state.render_tick),
                None => None,
            };
            let ks = KnowledgeState {
                zindeks,
                ingat,
                git,
                context_tokens,
                budget_tokens,
                zindeks_since_tick,
                ingat_since_tick,
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

/// Summarizes a tool-group header's stacked child names: `"{name} ×{n}"`
/// when every child shares one name (the common case — a burst of the same
/// tool, e.g. many `read_file` calls), else `"{n} tools"`. Pure so the
/// grouping rule is unit-testable without driving `apply_event`. `children`
/// is never empty in practice (the caller always pushes before summarizing),
/// but an empty slice degrades to `"0 tools"` rather than panicking.
pub(crate) fn tool_group_summary(children: &[String]) -> String {
    match children.split_first() {
        Some((first, rest)) if rest.iter().all(|n| n == first) => {
            format!("{first} \u{d7}{}", children.len())
        }
        _ => format!("{} tools", children.len()),
    }
}

/// Builds the Ledger view's WHY lines from a Knowledge digest: the first
/// zindeks fact and the first ingat memory, when present. No invented
/// captions — real event data only.
pub(crate) fn ledger_why_from(ks: &KnowledgeState) -> Vec<(WhySource, String)> {
    let mut why = Vec::new();
    if let Some(z) = ks.zindeks.first() {
        why.push((WhySource::Zindeks, z.clone()));
    }
    if let Some(i) = ks.ingat.first() {
        why.push((WhySource::Ingat, i.clone()));
    }
    why
}
