use std::time::{Duration, Instant};

use kode_core::event::{KodeEvent, NoteSource, TaskStep};

use super::draw::should_flush_stream_buffer;
use super::markdown;
use super::state::*;

/// Flushes all streamed model text into the stable transcript. Called before
/// non-token events and before a user steering line is inserted, preserving
/// chronological ordering even when steering lands mid-stream.
pub(crate) fn flush_model_stream(state: &mut AppState) {
    if !state.stream_pending.is_empty() {
        let pending = std::mem::take(&mut state.stream_pending);
        state.current_stream.push_str(&pending);
    }
    state.stream_last_flush = None;

    if !state.current_stream.is_empty() {
        let text = std::mem::take(&mut state.current_stream);
        state.response_buf.push_str(&text);
        for line in text.split('\n') {
            if line.is_empty() {
                state.transcript.push(TranscriptLine::new(Gutter::None, ""));
            } else {
                if !state.reply_label_shown {
                    state.reply_label_shown = true;
                    if !matches!(state.transcript.last(), Some(l) if l.gutter == Gutter::None && l.text.is_empty())
                    {
                        state.transcript.push(TranscriptLine::new(Gutter::None, ""));
                    }
                    state
                        .transcript
                        .push(TranscriptLine::new(Gutter::Reply, "KODE"));
                }
                let rendered = markdown::render_line(line, &mut state.md_in_code_block);
                if rendered.kind == markdown::MdKind::Heading
                    && !matches!(state.transcript.last(), Some(l) if matches!(l.gutter, Gutter::None | Gutter::Reply))
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
}

/// Applies one `KodeEvent` to `state`. Any accumulated `current_stream` text
/// is flushed into the transcript before non-token events are processed, so
/// the transcript always reads as a sequence of complete lines.
pub fn apply_event(state: &mut AppState, ev: KodeEvent) {
    if !matches!(ev, KodeEvent::ModelToken { .. }) {
        flush_model_stream(state);
    }

    state.ledger_recorder.observe(&ev);

    match ev {
        KodeEvent::AgentStarted => {
            state.running = true;
            state.steering_active = true;
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
            if !state.response_buf.is_empty() && !state.response_buf.ends_with('\n') {
                state.response_buf.push_str("\n\n");
            }
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
            state.pulse_tokens = state.pulse_tokens.saturating_add(1);
            state.stream_pending.push_str(&text);
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
            if !state.transcript.is_empty() {
                let last_index = state.transcript.len() - 1;
                state.touch_transcript(last_index);
            }
            match state.transcript.last_mut() {
                Some(last) if last.gutter == Gutter::Tool => {
                    if last.tool_children.is_empty() {
                        let prior = std::mem::take(&mut last.text);
                        last.tool_children.push(prior);
                    }
                    last.tool_children.push(name.clone());
                    last.text = tool_group_summary(&last.tool_children);
                    last.tool_ok = None;
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
            if !state.decide_marked_this_run {
                state.decide_marked_this_run = true;
                if let Some(entry) = state
                    .ledger
                    .steps
                    .iter_mut()
                    .find(|(step, _)| *step == TaskStep::Decide)
                {
                    entry.1 = true;
                    state
                        .ledger
                        .done_at
                        .push((TaskStep::Decide, Instant::now()));
                }
            }
        }
        KodeEvent::ToolFinished { name, ok, error } => {
            let elapsed_ms = state
                .tool_started
                .take()
                .map(|started| started.elapsed().as_millis())
                .unwrap_or(0);
            if let Some(idx) = state
                .transcript
                .iter()
                .rposition(|line| line.gutter == Gutter::Tool && line.tool_ok.is_none())
            {
                state.touch_transcript(idx);
                let receipt = &mut state.transcript[idx];
                receipt.tool_duration_ms = Some(
                    receipt
                        .tool_duration_ms
                        .unwrap_or(0)
                        .saturating_add(elapsed_ms),
                );
                receipt.tool_ok = Some(ok);
            }
            if !ok {
                let text = match error.as_deref().map(first_line_truncated) {
                    Some(reason) if !reason.is_empty() => format!("{name} failed: {reason}"),
                    _ => format!("{name} failed"),
                };
                state
                    .transcript
                    .push(TranscriptLine::new(Gutter::ToolFail, text));
            }
            state.current_tool = None;
        }
        KodeEvent::SubagentStarted { id, ownership } => {
            let scope = if ownership.is_empty() {
                "read-only".to_string()
            } else {
                format!("owns {}", ownership.join(", "))
            };
            state.transcript.push(TranscriptLine::new(
                Gutter::Tool,
                format!("subagent {id} · {scope}"),
            ));
        }
        KodeEvent::SubagentActivity { id, text } => {
            if let Some(idx) = state.transcript.iter().rposition(|line| {
                line.gutter == Gutter::Tool
                    && line.tool_ok.is_none()
                    && line.text.starts_with(&format!("subagent {id} ·"))
            }) {
                state.touch_transcript(idx);
                let receipt = &mut state.transcript[idx];
                receipt.tool_children.push(text);
                receipt.text = format!(
                    "subagent {id} · {} tool{}",
                    receipt.tool_children.len(),
                    if receipt.tool_children.len() == 1 {
                        ""
                    } else {
                        "s"
                    }
                );
            }
        }
        KodeEvent::SubagentFinished { id, ok, summary } => {
            let status = if ok { "done" } else { "failed" };
            if let Some(idx) = state.transcript.iter().rposition(|line| {
                line.gutter == Gutter::Tool
                    && line.tool_ok.is_none()
                    && line.text.starts_with(&format!("subagent {id} ·"))
            }) {
                state.touch_transcript(idx);
                let receipt = &mut state.transcript[idx];
                let count = receipt.tool_children.len();
                receipt.text = if count == 0 {
                    format!("subagent {id} · {status} · {summary}")
                } else {
                    format!(
                        "subagent {id} · {count} tool{} · {status} · {summary}",
                        if count == 1 { "" } else { "s" }
                    )
                };
                receipt.tool_ok = Some(ok);
            }
        }
        KodeEvent::SteeringAccepted { message } => {
            state.append_pending_steering(&message);
        }
        KodeEvent::SteeringDeferred { .. } => {}
        KodeEvent::VerificationStarted => {
            state.status.state = RunState::Verify;
            state.steering_active = false;
            state.current_tool = None;
        }
        KodeEvent::VerificationFinished { ok } => {
            if !ok {
                // Failure is state, not motion: applies under reduced motion too.
                state.trace_back = Some(None);
                state.style_epoch += 1;
            } else if !state.reduced_motion {
                state.trace_back = Some(Some(Instant::now() + Duration::from_millis(300)));
                state.style_epoch += 1;
            }
        }
        KodeEvent::AgentFinished => {
            // The pipeline may still be verifying or preparing a repair
            // agent. `TaskFinished` owns the transition to idle.
            state.status.state = RunState::Thinking;
            state.steering_active = false;
            state.current_tool = None;
        }
        KodeEvent::AgentError { message } => {
            state.last_error = Some(message.clone());
            state.completion = None;
            state
                .transcript
                .push(TranscriptLine::new(Gutter::Error, message));
            state.running = false;
            state.steering_active = false;
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
        KodeEvent::RouterDecision { answers } => {
            // The ledger was built from the user's plan_mode before routing;
            // a model "plan" answer turns plan mode on, so show the step.
            let router_plans = answers.iter().any(|a| {
                a.key == "plan"
                    && a.value == "plan"
                    && a.source == kode_core::event::RouteSource::Laya
            });
            if router_plans && state.ledger.steps.iter().all(|(s, _)| *s != TaskStep::Plan) {
                state.ledger.steps.insert(0, (TaskStep::Plan, false));
            }
            let mut line = TranscriptLine::new(Gutter::Route, route_line_text(&answers));
            line.born = Some(Instant::now());
            state.transcript.push(line);
        }
        KodeEvent::SourcedNote { text, source } => {
            let gutter = match source {
                NoteSource::Zindeks => Gutter::Zindeks,
                NoteSource::Ingat => Gutter::Ingat,
                NoteSource::Git => Gutter::Git,
            };
            if !state.thread_head_shown {
                state.thread_head_shown = true;
                state
                    .transcript
                    .push(TranscriptLine::new(Gutter::ThreadHead, ""));
            }
            let mut line = TranscriptLine::new(gutter, text);
            line.born = Some(Instant::now());
            state.transcript.push(line);
        }
        KodeEvent::TaskFinished {
            iterations,
            tool_calls,
            input_tokens,
            output_tokens,
            cached_tokens,
        } => {
            let elapsed_ms = state
                .run_started
                .map(|started| started.elapsed().as_millis())
                .unwrap_or(0);
            state.completion = Some(CompletionReceipt {
                iterations,
                tool_calls,
                input_tokens,
                output_tokens,
                cached_tokens,
                elapsed_ms,
                verify_steps: state.ledger.verify_steps.clone(),
                numstat: state.ledger.numstat.clone(),
            });
            state.last_error = None;
            state.running = false;
            state.steering_active = false;
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
                    format!("{name} · {dur:.1}s"),
                    StepStatusLite::Skipped,
                )
            } else if passed {
                (
                    Gutter::Verify,
                    format!("{name} · {dur:.1}s"),
                    StepStatusLite::Passed,
                )
            } else {
                (
                    Gutter::VerifyFail,
                    format!("{name} · {dur:.1}s"),
                    StepStatusLite::Failed,
                )
            };
            state.transcript.push(TranscriptLine::new(gutter, text));
            state.ledger.verify_steps.push((name, status));
        }
        KodeEvent::TaskProgress { step, done } => {
            if let Some(entry) = state.ledger.steps.iter_mut().find(|(s, _)| *s == step) {
                entry.1 = done;
                if done {
                    state.ledger.done_at.push((step, Instant::now()));
                }
            }
        }
        // The TUI's CURRENT CHANGE rows already come from the git poll; the
        // ledger recorder (P0 Task 4) is what consumes this event.
        KodeEvent::ChangeSet { .. } => {}
        KodeEvent::IndexStarted => {
            state.indexing_since = Some(Instant::now());
            state.index_line = Some(state.transcript.len());
            let mut line =
                TranscriptLine::new(Gutter::Zindeks, "◐ indexing this repo in the background");
            line.born = Some(Instant::now());
            state.transcript.push(line);
        }
        KodeEvent::IndexFinished {
            files,
            error,
            elapsed_ms,
        } => {
            state.indexing_since = None;
            let text = match (error, files) {
                (Some(error), _) => format!("✗ index failed: {error}"),
                (None, Some(files)) => format!(
                    "✓ indexed {} files · {}s",
                    crate::repo_map::thousands(files),
                    elapsed_ms / 1000
                ),
                (None, None) => format!("✓ indexed · {}s", elapsed_ms / 1000),
            };
            if let Some(row) = state.index_line {
                state.touch_transcript(row);
            }
            match state
                .index_line
                .and_then(|row| state.transcript.get_mut(row))
            {
                Some(line) => line.text = text,
                None => state
                    .transcript
                    .push(TranscriptLine::new(Gutter::Zindeks, text)),
            }
            state.transcript_cache = Default::default();
        }
        KodeEvent::GraphAnswered {
            query,
            latency_ms,
            text,
            ..
        } => {
            let mut line = TranscriptLine::new(
                Gutter::Zindeks,
                format!("╰▶ graph · {query} · 0 tokens · {latency_ms}ms"),
            );
            line.born = Some(Instant::now());
            state.transcript.push(line);
            state.response_buf = text;
            state.graph_offer = state.pending_task.clone();
        }
        KodeEvent::Impact {
            symbol,
            callers,
            crates,
            tests,
            ..
        } => {
            let mut line = TranscriptLine::new(
                Gutter::Zindeks,
                crate::impact::row_text_parts(&symbol, callers, crates, tests),
            );
            line.born = Some(Instant::now());
            state.transcript.push(line);
        }
    }
}

/// Text of the transcript's `Route` line: `route k=v · k=v` plus, for the
/// first Laya answer that reports confidence, a 10-cell `■□` bar and the
/// score. Static answers carry their reason inline and get no bar. Pure so
/// it is unit-testable without driving `apply_event`.
pub(crate) fn route_line_text(answers: &[kode_core::event::RouteAnswer]) -> String {
    use kode_core::event::RouteSource;
    let parts: Vec<String> = answers
        .iter()
        .map(|a| match &a.source {
            RouteSource::Laya => format!("{}={}", a.key, a.value),
            RouteSource::Static(reason) => format!("{}={} (static: {reason})", a.key, a.value),
        })
        .collect();
    let mut text = format!("route {}", parts.join(" · "));
    let laya = answers
        .iter()
        .find(|a| a.source == RouteSource::Laya && a.confidence.is_some());
    if let Some(confidence) = laya.and_then(|a| a.confidence) {
        let filled = ((confidence * 10.0).round().max(0.0) as usize).min(10);
        text.push_str("  ");
        text.extend((0..10).map(|i| if i < filled { '■' } else { '□' }));
        text.push_str(&format!(" {confidence:.2}"));
        if confidence < 0.5 {
            text.push_str(" low");
        }
    }
    text
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

/// First line of `s`, trimmed and clipped to 160 chars (with `…`), so a tool
/// failure reason fits on one transcript row.
pub(crate) fn first_line_truncated(s: &str) -> String {
    const MAX: usize = 160;
    let line = s.lines().next().unwrap_or("").trim();
    if line.chars().count() <= MAX {
        return line.to_string();
    }
    let mut out: String = line.chars().take(MAX - 1).collect();
    out.push('\u{2026}');
    out
}

#[cfg(test)]
mod subagent_group_tests {
    use super::*;

    #[test]
    fn subagent_activities_collapse_into_one_expandable_tool_receipt() {
        let mut state = AppState::new("codex".into(), "model".into(), String::new());
        apply_event(
            &mut state,
            KodeEvent::SubagentStarted {
                id: "worker".into(),
                ownership: vec![],
            },
        );
        for text in ["code_search", "read_file"] {
            apply_event(
                &mut state,
                KodeEvent::SubagentActivity {
                    id: "worker".into(),
                    text: text.into(),
                },
            );
        }
        apply_event(
            &mut state,
            KodeEvent::SubagentFinished {
                id: "worker".into(),
                ok: true,
                summary: "found the seam".into(),
            },
        );

        assert_eq!(state.transcript.len(), 1);
        assert_eq!(state.transcript[0].gutter, Gutter::Tool);
        assert_eq!(
            state.transcript[0].tool_children,
            ["code_search", "read_file"]
        );
        assert!(state.transcript[0].text.contains("2 tools · done"));
        assert_eq!(state.transcript[0].tool_ok, Some(true));
    }
}
