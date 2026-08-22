use super::*;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::custom_commands;
use crossterm::event::{KeyCode, KeyModifiers, MouseEventKind};
use kode_context::git::{NumstatRow, RepoState};
use kode_core::config::KodeConfig;
use kode_core::event::{KodeEvent, NoteSource, TaskStep};
use kode_tools::permission::PermissionHandler;
use ratatui::style::Modifier;
use ratatui::text::Line;
use tokio::sync::{mpsc, oneshot};

fn state() -> AppState {
    AppState::new("openai".to_string(), "gpt-test".to_string(), String::new())
}

#[test]
fn model_token_buffers_short_chunks_until_boundary() {
    let mut s = state();
    apply_event(&mut s, KodeEvent::ModelToken { text: "hel".into() });
    apply_event(&mut s, KodeEvent::ModelToken { text: "lo".into() });
    // "hello" has no word/whitespace boundary yet and is well under
    // the 120ms coalescing window — buffered, not yet visible.
    assert_eq!(s.current_stream, "");
    assert!(s.transcript.is_empty());

    apply_event(&mut s, KodeEvent::ModelToken { text: " ".into() });
    // Trailing whitespace is a boundary — flush now.
    assert_eq!(s.current_stream, "hello ");

    apply_event(
        &mut s,
        KodeEvent::ModelToken {
            text: "world".into(),
        },
    );
    // New chunk buffers again until the next boundary/flush.
    assert_eq!(s.current_stream, "hello ");
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
            error: Some("boom".into()),
        },
    );
    assert_eq!(
        s.transcript,
        vec![TranscriptLine::new(
            Gutter::ToolFail,
            "run_shell failed: boom"
        )]
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
            error: None,
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
            error: None,
        },
    );
    assert!(s.transcript.is_empty());
}

#[test]
fn tool_started_consecutive_same_name_groups_with_count() {
    let mut s = state();
    apply_event(
        &mut s,
        KodeEvent::ToolStarted {
            name: "read_file".into(),
        },
    );
    apply_event(
        &mut s,
        KodeEvent::ToolStarted {
            name: "read_file".into(),
        },
    );
    assert_eq!(s.transcript.len(), 1);
    assert_eq!(s.transcript[0].gutter, Gutter::Tool);
    assert_eq!(s.transcript[0].text, "read_file \u{d7}2");
    assert_eq!(
        s.transcript[0].tool_children,
        vec!["read_file".to_string(), "read_file".to_string()]
    );
}

#[test]
fn tool_started_mixed_names_groups_as_n_tools() {
    let mut s = state();
    apply_event(
        &mut s,
        KodeEvent::ToolStarted {
            name: "read_file".into(),
        },
    );
    apply_event(
        &mut s,
        KodeEvent::ToolStarted {
            name: "write_file".into(),
        },
    );
    assert_eq!(s.transcript.len(), 1);
    assert_eq!(s.transcript[0].text, "2 tools");
    assert_eq!(
        s.transcript[0].tool_children,
        vec!["read_file".to_string(), "write_file".to_string()]
    );
}

#[test]
fn tool_group_broken_by_non_tool_line() {
    let mut s = state();
    apply_event(
        &mut s,
        KodeEvent::ToolStarted {
            name: "read_file".into(),
        },
    );
    apply_event(
        &mut s,
        KodeEvent::Note {
            text: "interruption".into(),
        },
    );
    apply_event(
        &mut s,
        KodeEvent::ToolStarted {
            name: "write_file".into(),
        },
    );
    let tool_lines: Vec<_> = s
        .transcript
        .iter()
        .filter(|l| l.gutter == Gutter::Tool)
        .collect();
    assert_eq!(tool_lines.len(), 2);
    assert_eq!(tool_lines[0].text, "read_file");
    assert!(tool_lines[0].tool_children.is_empty());
    assert_eq!(tool_lines[1].text, "write_file");
    assert!(tool_lines[1].tool_children.is_empty());
}

#[test]
fn tool_group_summary_single_vs_mixed() {
    assert_eq!(
        tool_group_summary(&["read_file".to_string(), "read_file".to_string()]),
        "read_file \u{d7}2"
    );
    assert_eq!(
        tool_group_summary(&["read_file".to_string(), "write_file".to_string()]),
        "2 tools"
    );
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
        ..Default::default()
    };
    let lines = knowledge_band_lines(&ks, 5, false);
    assert_eq!(lines.len(), 3);
    assert!(line_text(&lines[0]).contains("+1 more"));
    assert!(line_text(&lines[1]).contains("+1 more"));
    assert!(!line_text(&lines[2]).contains("more"));
}

#[test]
fn knowledge_event_tracks_zindeks_row_insertion_tick_and_preserves_when_unchanged() {
    let mut s = state();
    s.render_tick = 3;
    apply_event(
        &mut s,
        KodeEvent::Knowledge {
            zindeks: vec!["src/foo.rs".to_string()],
            ingat: vec![],
            git: vec![],
            context_tokens: 100,
            budget_tokens: 1000,
        },
    );
    assert_eq!(s.knowledge.as_ref().unwrap().zindeks_since_tick, Some(3));

    s.render_tick = 4;
    apply_event(
        &mut s,
        KodeEvent::Knowledge {
            zindeks: vec!["src/foo.rs".to_string()],
            ingat: vec![],
            git: vec![],
            context_tokens: 100,
            budget_tokens: 1000,
        },
    );
    // Same first fact — insertion tick preserved, not bumped.
    assert_eq!(s.knowledge.as_ref().unwrap().zindeks_since_tick, Some(3));

    s.render_tick = 9;
    apply_event(
        &mut s,
        KodeEvent::Knowledge {
            zindeks: vec!["src/bar.rs".to_string()],
            ingat: vec![],
            git: vec![],
            context_tokens: 100,
            budget_tokens: 1000,
        },
    );
    // New fact — insertion tick resets to the current render_tick.
    assert_eq!(s.knowledge.as_ref().unwrap().zindeks_since_tick, Some(9));
}

#[test]
fn evidence_row_dim_true_within_first_two_ticks() {
    assert!(evidence_row_dim(0, Some(0), false));
    assert!(evidence_row_dim(1, Some(0), false));
}

#[test]
fn evidence_row_dim_false_from_third_tick() {
    assert!(!evidence_row_dim(2, Some(0), false));
}

#[test]
fn evidence_row_dim_false_when_reduced_motion() {
    assert!(!evidence_row_dim(0, Some(0), true));
}

#[test]
fn evidence_row_dim_false_when_no_insertion_tick() {
    assert!(!evidence_row_dim(5, None, false));
}

#[test]
fn should_flush_stream_buffer_true_on_whitespace_boundary() {
    assert!(should_flush_stream_buffer(
        "hello ",
        Duration::from_millis(0)
    ));
}

#[test]
fn should_flush_stream_buffer_true_after_120ms_even_mid_word() {
    assert!(should_flush_stream_buffer(
        "hel",
        Duration::from_millis(120)
    ));
}

#[test]
fn should_flush_stream_buffer_false_when_mid_word_and_recent() {
    assert!(!should_flush_stream_buffer(
        "hel",
        Duration::from_millis(50)
    ));
}

#[test]
fn should_flush_stream_buffer_false_when_empty() {
    assert!(!should_flush_stream_buffer("", Duration::from_millis(999)));
}

#[test]
fn spinner_glyph_animates_normally() {
    assert_eq!(spinner_glyph(0, false, false), spinner_frame(0));
    assert_eq!(spinner_glyph(250, false, false), spinner_frame(250));
}

#[test]
fn spinner_glyph_static_when_reduced_motion() {
    assert_eq!(spinner_glyph(250, true, false), SPINNER_FRAMES[2]);
    assert_eq!(spinner_glyph(500, true, false), SPINNER_FRAMES[2]);
}

#[test]
fn spinner_glyph_static_while_streaming() {
    assert_eq!(spinner_glyph(250, false, true), SPINNER_FRAMES[2]);
    assert_eq!(spinner_glyph(500, false, true), SPINNER_FRAMES[2]);
}

#[test]
fn ledger_pulse_glyph_static_when_idle() {
    assert_eq!(ledger_pulse_glyph(250, false, false), '●');
    assert_eq!(ledger_pulse_glyph(500, false, false), '●');
}

#[test]
fn ledger_pulse_glyph_static_when_reduced_motion() {
    assert_eq!(ledger_pulse_glyph(250, true, true), '●');
}

#[test]
fn ledger_pulse_glyph_alternates_at_4hz_while_running() {
    assert_eq!(ledger_pulse_glyph(0, true, false), '●');
    assert_eq!(ledger_pulse_glyph(250, true, false), '◉');
    assert_eq!(ledger_pulse_glyph(500, true, false), '●');
    assert_eq!(ledger_pulse_glyph(750, true, false), '◉');
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
    let quit = handle_key(
        &mut s,
        std::path::Path::new("."),
        KeyCode::Char('y'),
        KeyModifiers::NONE,
        &None,
    );
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
    let quit = handle_key(
        &mut s,
        std::path::Path::new("."),
        KeyCode::Char('n'),
        KeyModifiers::NONE,
        &None,
    );
    assert!(!quit);
    assert!(!rx.await.unwrap());
}

#[test]
fn handle_key_q_quits_when_idle_and_input_empty() {
    let mut s = state();
    assert!(handle_key(
        &mut s,
        std::path::Path::new("."),
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
        std::path::Path::new("."),
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
    handle_key(
        &mut s,
        std::path::Path::new("."),
        KeyCode::Backspace,
        KeyModifiers::NONE,
        &None,
    );
    assert_eq!(s.input, "ab");
}

#[test]
fn handle_key_scroll_updates_position_and_follow() {
    let mut s = state();
    s.scroll = 5;
    handle_key(
        &mut s,
        std::path::Path::new("."),
        KeyCode::Up,
        KeyModifiers::NONE,
        &None,
    );
    assert_eq!(s.scroll, 4);
    assert!(!s.follow);
    handle_key(
        &mut s,
        std::path::Path::new("."),
        KeyCode::PageDown,
        KeyModifiers::NONE,
        &None,
    );
    assert_eq!(s.scroll, 14);
}

#[test]
fn wheel_delta_maps_scroll_kinds_only() {
    assert_eq!(wheel_delta(MouseEventKind::ScrollUp), -3);
    assert_eq!(wheel_delta(MouseEventKind::ScrollDown), 3);
    assert_eq!(wheel_delta(MouseEventKind::Moved), 0);
    assert_eq!(
        wheel_delta(MouseEventKind::Down(crossterm::event::MouseButton::Left)),
        0
    );
}

/// Builds a full `MouseEvent` for the given `kind` at (0, 0) with no
/// modifiers — the tests below only care about `kind` unless noted.
fn mouse_event(kind: MouseEventKind) -> crossterm::event::MouseEvent {
    crossterm::event::MouseEvent {
        kind,
        column: 0,
        row: 0,
        modifiers: KeyModifiers::NONE,
    }
}

#[test]
fn handle_mouse_scroll_updates_position_and_follow() {
    let mut s = state();
    s.scroll = 5;
    handle_mouse(&mut s, mouse_event(MouseEventKind::ScrollUp));
    assert_eq!(s.scroll, 2);
    assert!(!s.follow);
    handle_mouse(&mut s, mouse_event(MouseEventKind::ScrollDown));
    assert_eq!(s.scroll, 5);
}

#[test]
fn handle_mouse_scroll_up_clamps_at_zero() {
    let mut s = state();
    s.scroll = 1;
    handle_mouse(&mut s, mouse_event(MouseEventKind::ScrollUp));
    assert_eq!(s.scroll, 0);
}

#[test]
fn handle_mouse_ignores_non_wheel_events() {
    let mut s = state();
    s.scroll = 5;
    handle_mouse(
        &mut s,
        mouse_event(MouseEventKind::Down(crossterm::event::MouseButton::Left)),
    );
    assert_eq!(s.scroll, 5);
}

#[test]
fn hit_test_row_maps_wrapped_rows_to_header() {
    let rows: Vec<(u16, Option<usize>)> = vec![(2, None), (3, Some(5)), (1, None)];
    assert_eq!(hit_test_row(&rows, 0), None);
    assert_eq!(hit_test_row(&rows, 1), None);
    assert_eq!(hit_test_row(&rows, 2), Some(5));
    assert_eq!(hit_test_row(&rows, 3), Some(5));
    assert_eq!(hit_test_row(&rows, 4), Some(5));
    assert_eq!(hit_test_row(&rows, 5), None);
    // Past the end of the rendered content.
    assert_eq!(hit_test_row(&rows, 6), None);
}

#[test]
fn handle_mouse_left_click_toggles_group_header_under_cursor() {
    let mut s = state();
    s.transcript
        .push(TranscriptLine::new(Gutter::Tool, "read_file \u{d7}2"));
    s.transcript[0].tool_children = vec!["read_file".to_string(), "read_file".to_string()];
    assert!(!s.transcript[0].expanded);

    s.transcript_hit = Some(TranscriptHit {
        area: ratatui::layout::Rect {
            x: 0,
            y: 0,
            width: 20,
            height: 5,
        },
        scroll: 0,
        rows: vec![(1, Some(0))],
    });

    handle_mouse(
        &mut s,
        crossterm::event::MouseEvent {
            kind: MouseEventKind::Down(crossterm::event::MouseButton::Left),
            column: 2,
            row: 0,
            modifiers: KeyModifiers::NONE,
        },
    );
    assert!(s.transcript[0].expanded);

    // Clicking again collapses it back.
    handle_mouse(
        &mut s,
        crossterm::event::MouseEvent {
            kind: MouseEventKind::Down(crossterm::event::MouseButton::Left),
            column: 2,
            row: 0,
            modifiers: KeyModifiers::NONE,
        },
    );
    assert!(!s.transcript[0].expanded);
}

#[test]
fn line_rows_counts_wrapped_line() {
    let line = Line::from(ratatui::text::Span::raw("a".repeat(25)));
    assert_eq!(line_rows(&line, 10), 3);
    assert_eq!(line_rows(&line, 0), 0);
}

#[test]
fn ctrl_t_toggles_select_mode_and_notes() {
    let mut s = state();
    assert!(!s.select_mode);

    let quit = handle_key(
        &mut s,
        std::path::Path::new("."),
        KeyCode::Char('t'),
        KeyModifiers::CONTROL,
        &None,
    );
    assert!(!quit);
    assert!(s.select_mode);
    // Mode indicator lives on the input line, never in the transcript.
    assert!(
        !s.transcript
            .iter()
            .any(|l| l.gutter == Gutter::Note && l.text.contains("select"))
    );

    handle_key(
        &mut s,
        std::path::Path::new("."),
        KeyCode::Char('t'),
        KeyModifiers::CONTROL,
        &None,
    );
    assert!(!s.select_mode);
    let note_count = s
        .transcript
        .iter()
        .filter(|l| l.gutter == Gutter::Note)
        .count();
    assert_eq!(note_count, 0);
}

#[test]
fn clamp_scroll_content_fits_clamps_to_zero() {
    assert_eq!(clamp_scroll(0, 5, 10), 0);
    assert_eq!(clamp_scroll(3, 5, 10), 0);
}

#[test]
fn clamp_scroll_overflow_keeps_in_range_position() {
    // total=100, viewport=10 -> max_scroll=90; a mid-range position is
    // untouched by clamping.
    assert_eq!(clamp_scroll(50, 100, 10), 50);
}

#[test]
fn clamp_scroll_past_end_clamps_to_max() {
    assert_eq!(clamp_scroll(1000, 100, 10), 90);
}

#[test]
fn clamp_scroll_u16_edges() {
    // viewport taller than content: saturating_sub avoids underflow.
    assert_eq!(clamp_scroll(u16::MAX, 5, 20), 0);
    // total_lines and scroll both at the u16 ceiling.
    assert_eq!(clamp_scroll(u16::MAX, u16::MAX, 0), u16::MAX);
    // zero everywhere.
    assert_eq!(clamp_scroll(0, 0, 0), 0);
}

#[test]
fn scrollbar_state_hidden_when_content_fits() {
    assert_eq!(scrollbar_state(5, 10, 0), None);
    assert_eq!(scrollbar_state(10, 10, 0), None);
}

#[test]
fn scrollbar_state_visible_reports_length_and_clamped_position() {
    assert_eq!(scrollbar_state(100, 10, 50), Some((100, 50)));
    // scroll past the end still reports the clamped position, not the
    // raw (out of range) one.
    assert_eq!(scrollbar_state(100, 10, 1000), Some((100, 90)));
}

#[test]
fn handle_key_ctrl_k_toggles_knowledge_band() {
    let mut s = state();
    assert!(s.knowledge_band_open);
    handle_key(
        &mut s,
        std::path::Path::new("."),
        KeyCode::Char('k'),
        KeyModifiers::CONTROL,
        &None,
    );
    assert!(!s.knowledge_band_open);
    handle_key(
        &mut s,
        std::path::Path::new("."),
        KeyCode::Char('k'),
        KeyModifiers::CONTROL,
        &None,
    );
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
    assert_eq!(gutter_prefix(&Gutter::Zindeks).0, "Z ");
    assert_eq!(gutter_prefix(&Gutter::Ingat).0, "I ");
    assert_eq!(gutter_prefix(&Gutter::Git).0, "G ");
    assert_eq!(gutter_prefix(&Gutter::Error).0, "× ");
    assert_eq!(gutter_prefix(&Gutter::User).0, "U ");
}

#[test]
fn user_turn_is_bold_while_agent_prose_is_not() {
    let user = transcript_line_to_ratatui(&TranscriptLine::new(Gutter::User, "fix the bug"));
    let agent = transcript_line_to_ratatui(&TranscriptLine::new(Gutter::Prose, "fixed"));

    assert!(user.spans[0].style.add_modifier.contains(Modifier::BOLD));
    assert!(user.spans[1].style.add_modifier.contains(Modifier::BOLD));
    assert!(!agent.spans[0].style.add_modifier.contains(Modifier::BOLD));
    assert!(!agent.spans[1].style.add_modifier.contains(Modifier::BOLD));
}

#[test]
fn gutter_prefix_zindeks_ingat_git_use_distinct_colors() {
    assert_eq!(gutter_prefix(&Gutter::Zindeks).1, theme::Z);
    assert_eq!(gutter_prefix(&Gutter::Ingat).1, theme::I);
    assert_eq!(gutter_prefix(&Gutter::Git).1, theme::DIM);
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
fn parse_slash_command_plan() {
    assert_eq!(parse_slash_command("/plan"), Some(SlashCommand::Plan));
}

#[test]
fn parse_slash_command_non_builtin_name_is_custom() {
    // Non-builtin names parse as `Custom` — resolved against discovered
    // commands at handle time, not parse time. An unmatched name still
    // ends up producing the same "unknown command" transcript line
    // `Unknown` used to (see `handle_slash_command_custom_unmatched_...`).
    assert_eq!(
        parse_slash_command("/nonsense arg"),
        Some(SlashCommand::Custom {
            name: "nonsense".to_string(),
            args: "arg".to_string(),
        })
    );
}

#[test]
fn parse_slash_command_custom_name_normalized_lowercase() {
    assert_eq!(
        parse_slash_command("/Review some diff"),
        Some(SlashCommand::Custom {
            name: "review".to_string(),
            args: "some diff".to_string(),
        })
    );
}

#[test]
fn parse_slash_command_bare_slash_is_unknown() {
    assert_eq!(
        parse_slash_command("/"),
        Some(SlashCommand::Unknown("/".to_string()))
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
        provider_auth_state("codex", true, &[], false, false, false),
        " ✓ logged in"
    );
}

#[test]
fn provider_auth_state_codex_not_logged_in() {
    assert_eq!(
        provider_auth_state("codex", false, &[], false, false, false),
        ""
    );
}

#[test]
fn provider_auth_state_opencode_family_matches_key() {
    let keys = vec!["opencode-go".to_string()];
    assert_eq!(
        provider_auth_state("opencode-go", false, &keys, false, false, false),
        " ✓ logged in"
    );
    assert_eq!(
        provider_auth_state("opencode", false, &keys, false, false, false),
        ""
    );
    assert_eq!(
        provider_auth_state("kilo", false, &keys, false, false, false),
        ""
    );
}

#[test]
fn provider_auth_state_openai_uses_env_key() {
    assert_eq!(
        provider_auth_state("openai", false, &[], true, false, false),
        " ✓ logged in"
    );
    assert_eq!(
        provider_auth_state("openai", false, &[], false, false, false),
        ""
    );
}

#[test]
fn provider_auth_state_anthropic_uses_auth_present() {
    assert_eq!(
        provider_auth_state("anthropic", false, &[], false, true, false),
        " ✓ logged in"
    );
    assert_eq!(
        provider_auth_state("anthropic", false, &[], false, false, false),
        ""
    );
}

#[test]
fn provider_auth_state_antigravity_uses_auth_present() {
    assert_eq!(
        provider_auth_state("antigravity", false, &[], false, false, true),
        " ✓ logged in"
    );
    assert_eq!(
        provider_auth_state("antigravity", false, &[], false, false, false),
        ""
    );
}

#[test]
fn provider_auth_state_lmstudio_always_local() {
    assert_eq!(
        provider_auth_state("lmstudio", false, &[], false, false, false),
        " (local)"
    );
    assert_eq!(
        provider_auth_state("lmstudio", true, &[], true, true, false),
        " (local)"
    );
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
    let dir = std::env::temp_dir().join(format!("kode-tui-test-{}-{nanos}", std::process::id()));
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
    assert!(s.transcript.iter().any(|l| l.text.contains("/plan")));
    assert!(s.transcript.iter().any(|l| l.text.contains("Ctrl+L")));
}

#[test]
fn handle_slash_command_plan_toggles_state() {
    let dir = temp_project_dir();
    let mut s = state();
    let mut cfg = KodeConfig::default();
    let (tx, _rx) = mpsc::unbounded_channel();

    assert!(!s.plan_mode);
    handle_slash_command(&mut s, &dir, &mut cfg, &tx, SlashCommand::Plan);
    assert!(s.plan_mode);
    assert!(s.transcript.iter().any(|l| l.text.contains("plan mode on")));

    handle_slash_command(&mut s, &dir, &mut cfg, &tx, SlashCommand::Plan);
    assert!(!s.plan_mode);
    assert!(s.transcript.iter().any(|l| l.text == "plan mode off"));
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
    s.start_new_task("second task", false);
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

#[test]
fn start_new_task_prepends_plan_step_only_when_plan_mode_is_on() {
    let mut s = state();

    s.start_new_task("no plan", false);
    assert!(
        !s.ledger
            .steps
            .iter()
            .any(|(step, _)| *step == TaskStep::Plan)
    );
    assert_eq!(s.ledger.steps[0].0, TaskStep::Understand);

    s.start_new_task("with plan", true);
    assert_eq!(s.ledger.steps[0], (TaskStep::Plan, false));
    assert_eq!(s.ledger.steps[1].0, TaskStep::Understand);
    assert_eq!(s.ledger.steps.len(), 5);
}

// -- ledger numstat / dirty poll -----------------------------------------

#[test]
fn tool_finished_never_touches_ledger_numstat() {
    // CURRENT CHANGE rows come solely from the lazy git poll
    // (`apply_repo_state`), not from counting tool calls — ToolFinished
    // (success, failure, apply_patch, or otherwise) is a no-op for it.
    let mut s = state();
    apply_event(
        &mut s,
        KodeEvent::ToolFinished {
            name: "apply_patch".into(),
            ok: true,
            error: None,
        },
    );
    apply_event(
        &mut s,
        KodeEvent::ToolFinished {
            name: "write_file".into(),
            ok: true,
            error: None,
        },
    );
    apply_event(
        &mut s,
        KodeEvent::ToolFinished {
            name: "apply_patch".into(),
            ok: false,
            error: Some("boom".into()),
        },
    );
    assert!(s.ledger.numstat.is_empty());
}

#[test]
fn apply_repo_state_sets_dirty_and_ledger_numstat() {
    let mut s = state();
    assert!(!s.dirty);
    assert!(s.ledger.numstat.is_empty());

    apply_repo_state(
        &mut s,
        RepoState {
            dirty: true,
            numstat: vec![NumstatRow {
                path: "src/foo.rs".to_string(),
                added: 3,
                deleted: 1,
            }],
        },
    );

    assert!(s.dirty);
    assert_eq!(s.ledger.numstat.len(), 1);
    assert_eq!(s.ledger.numstat[0].path, "src/foo.rs");
}

#[test]
fn numstat_caption_none_when_empty_some_with_file_count() {
    assert_eq!(numstat_caption(&[]), None);
    let rows = vec![
        NumstatRow {
            path: "a.rs".to_string(),
            added: 1,
            deleted: 0,
        },
        NumstatRow {
            path: "b.rs".to_string(),
            added: 0,
            deleted: 2,
        },
    ];
    assert_eq!(numstat_caption(&rows), Some("2 files changed".to_string()));
}

#[test]
fn split_ingat_confidence_extracts_dim_suffix() {
    assert_eq!(
        split_ingat_confidence("always prefix with rtk \u{2504} 0.87"),
        ("always prefix with rtk", Some("0.87"))
    );
    assert_eq!(
        split_ingat_confidence("always prefix with rtk"),
        ("always prefix with rtk", None)
    );
}

#[test]
fn breadcrumb_line_shows_dim_asterisk_when_dirty() {
    let mut s = state();
    s.branch = Some("main".to_string());
    s.dirty = true;
    let text = line_text(&breadcrumb_line(&s));
    assert!(text.contains("main*"));
}

#[test]
fn breadcrumb_line_no_asterisk_when_clean() {
    let mut s = state();
    s.branch = Some("main".to_string());
    s.dirty = false;
    let text = line_text(&breadcrumb_line(&s));
    assert!(!text.contains("main*"));
    assert!(text.contains("main "));
}

#[test]
fn sourced_note_pushes_matching_gutter() {
    let mut s = state();
    apply_event(
        &mut s,
        KodeEvent::SourcedNote {
            text: "zindeks index refreshed".into(),
            source: NoteSource::Zindeks,
        },
    );
    apply_event(
        &mut s,
        KodeEvent::SourcedNote {
            text: "engineering memory unavailable".into(),
            source: NoteSource::Ingat,
        },
    );
    assert_eq!(
        s.transcript,
        vec![
            TranscriptLine::new(Gutter::Zindeks, "zindeks index refreshed"),
            TranscriptLine::new(Gutter::Ingat, "engineering memory unavailable"),
        ]
    );
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
    s.start_new_task("next task", false);
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
        ..Default::default()
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

    handle_key(
        &mut s,
        std::path::Path::new("."),
        KeyCode::BackTab,
        KeyModifiers::NONE,
        &None,
    );
    assert!(s.auto_mode);
    assert!(s.auto_flag.load(Ordering::Relaxed));
    assert!(s.transcript.iter().any(|l| l.text.contains("auto mode on")));

    handle_key(
        &mut s,
        std::path::Path::new("."),
        KeyCode::BackTab,
        KeyModifiers::NONE,
        &None,
    );
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

#[test]
fn breadcrumb_line_shows_plan_badge_only_when_on() {
    let mut s = state();
    assert!(!line_text(&breadcrumb_line(&s)).contains("plan"));
    s.plan_mode = true;
    assert!(line_text(&breadcrumb_line(&s)).contains("· plan"));
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
    handle_key(
        &mut s,
        std::path::Path::new("."),
        KeyCode::Char('y'),
        KeyModifiers::CONTROL,
        &None,
    );
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
    assert!(s.transcript.iter().any(|l| l.text.contains("Ctrl+T")));
}

// -- /resume picker ----------------------------------------------------

#[test]
fn parse_slash_command_resume() {
    assert_eq!(parse_slash_command("/resume"), Some(SlashCommand::Resume));
}

#[test]
fn resume_picker_session_id_extraction() {
    assert_eq!(
        session_id_from_row("20260817-010101  fix the bug  · 3 turns"),
        "20260817-010101"
    );
}

#[test]
fn handle_slash_command_resume_with_no_sessions_notes() {
    let dir = temp_project_dir();
    let mut s = state();
    let mut cfg = KodeConfig::default();
    let (tx, _rx) = mpsc::unbounded_channel();

    handle_slash_command(&mut s, &dir, &mut cfg, &tx, SlashCommand::Resume);

    assert!(!s.picker.open);
    assert!(
        s.transcript
            .iter()
            .any(|l| l.text == "no sessions to resume")
    );
}

#[test]
fn handle_slash_command_resume_with_sessions_opens_picker() {
    let dir = temp_project_dir();
    let id = crate::session::create(&dir, "codex", "gpt-test").unwrap();
    crate::session::append_turn(
        &dir,
        &id,
        &crate::session::Turn {
            ts: "2026-08-17T00:00:00Z".to_string(),
            task: "fix the bug".to_string(),
            response: "done".to_string(),
            tool_calls: 1,
        },
    )
    .unwrap();

    let mut s = state();
    let mut cfg = KodeConfig::default();
    let (tx, _rx) = mpsc::unbounded_channel();

    handle_slash_command(&mut s, &dir, &mut cfg, &tx, SlashCommand::Resume);

    assert!(s.picker.open);
    assert_eq!(s.picker.kind, PickerKind::Session);
    assert_eq!(s.picker.items.len(), 1);
    assert!(s.picker.items[0].starts_with(&id));
    assert!(s.picker.items[0].contains("fix the bug"));
}

// -- handle_slash_command: custom commands -----------------------------

fn write_custom_command(dir: &std::path::Path, name: &str, content: &str) {
    let cmds = dir.join(".kode").join("commands");
    std::fs::create_dir_all(&cmds).unwrap();
    std::fs::write(cmds.join(format!("{name}.md")), content).unwrap();
}

#[test]
fn handle_slash_command_custom_submits_expanded_prompt() {
    let dir = temp_project_dir();
    write_custom_command(&dir, "review", "Review carefully: $ARGUMENTS");
    let mut s = state();
    let mut cfg = KodeConfig::default();
    let (tx, _rx) = mpsc::unbounded_channel();

    let submitted = handle_slash_command(
        &mut s,
        &dir,
        &mut cfg,
        &tx,
        SlashCommand::Custom {
            name: "review".to_string(),
            args: "the diff".to_string(),
        },
    );

    assert_eq!(submitted, Some("Review carefully: the diff".to_string()));
}

#[test]
fn handle_slash_command_custom_unmatched_name_reports_unknown() {
    let dir = temp_project_dir();
    let mut s = state();
    let mut cfg = KodeConfig::default();
    let (tx, _rx) = mpsc::unbounded_channel();

    let submitted = handle_slash_command(
        &mut s,
        &dir,
        &mut cfg,
        &tx,
        SlashCommand::Custom {
            name: "nope".to_string(),
            args: String::new(),
        },
    );

    assert_eq!(submitted, None);
    assert!(
        s.transcript
            .iter()
            .any(|l| l.text.contains("unknown command: /nope"))
    );
}

#[test]
fn handle_slash_command_builtin_name_never_shadowed_by_custom_file() {
    // A `.kode/commands/help.md` file exists, but `/help` still parses to
    // the builtin `SlashCommand::Help` (matched literally before any
    // custom-command lookup happens), so it keeps producing the builtin
    // help text rather than being submitted as an expanded task.
    let dir = temp_project_dir();
    write_custom_command(&dir, "help", "this should never run");
    let mut s = state();
    let mut cfg = KodeConfig::default();
    let (tx, _rx) = mpsc::unbounded_channel();

    assert_eq!(parse_slash_command("/help"), Some(SlashCommand::Help));

    let submitted = handle_slash_command(&mut s, &dir, &mut cfg, &tx, SlashCommand::Help);

    assert_eq!(submitted, None);
    assert!(s.transcript.iter().any(|l| l.text.contains("/model")));
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
    s.start_new_task("next task", false);
    assert_eq!(s.last_response, "prior answer");
}

// -- session persistence (record_completed_turn) ---------------------------

#[test]
fn task_finished_appends_turn_to_in_memory_history() {
    let dir = temp_project_dir();
    let mut s = state();

    s.start_new_task("first task", false);
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
    s.start_new_task("second task", false);
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

    s.start_new_task("will fail", false);
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

// -- resume (restore_session) ----------------------------------------------

#[test]
fn restore_session_replays_transcript_and_history() {
    let dir = temp_project_dir();
    let id = crate::session::create(&dir, "codex", "gpt-test").unwrap();
    crate::session::append_turn(
        &dir,
        &id,
        &crate::session::Turn {
            ts: "2026-08-17T00:00:00Z".to_string(),
            task: "first task".to_string(),
            response: "first answer".to_string(),
            tool_calls: 1,
        },
    )
    .unwrap();
    crate::session::append_turn(
        &dir,
        &id,
        &crate::session::Turn {
            ts: "2026-08-17T00:01:00Z".to_string(),
            task: "second task".to_string(),
            response: "second answer".to_string(),
            tool_calls: 0,
        },
    )
    .unwrap();

    let mut s = state();
    let ok = restore_session(&mut s, &dir, &id);
    assert!(ok);

    assert_eq!(s.transcript[0].gutter, Gutter::Note);
    assert!(s.transcript[0].text.contains(&id));
    assert!(s.transcript[0].text.contains("2 turns"));
    let user_lines: Vec<&TranscriptLine> = s
        .transcript
        .iter()
        .filter(|l| l.gutter == Gutter::User)
        .collect();
    assert_eq!(user_lines.len(), 2);
    assert_eq!(user_lines[0].text, "first task");
    assert_eq!(user_lines[1].text, "second task");
    assert_eq!(s.history.len(), 2);
    assert_eq!(s.session_id.as_deref(), Some(id.as_str()));
    assert_eq!(s.last_response, "second answer");
}

#[test]
fn restore_session_missing_file_notes_and_returns_false() {
    let dir = temp_project_dir();
    let mut s = state();
    let ok = restore_session(&mut s, &dir, "20260817-000000");
    assert!(!ok);
    assert!(
        s.transcript
            .iter()
            .any(|l| l.text.contains("could not load session"))
    );
    assert!(s.session_id.is_none());
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
    assert_eq!(slash_hint_items("/", &[]).len(), SLASH_COMMANDS.len());
    assert!(SLASH_COMMANDS.iter().any(|(name, _)| *name == "/resume"));
}

#[test]
fn slash_hint_items_prefix_narrows() {
    let items = slash_hint_items("/mo", &[]);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].0, "/model");
}

#[test]
fn slash_hint_items_non_slash_is_empty() {
    assert!(slash_hint_items("hello", &[]).is_empty());
}

#[test]
fn slash_hint_items_hides_once_args_typed() {
    assert!(slash_hint_items("/model g", &[]).is_empty());
}

#[test]
fn slash_hint_items_merges_custom_after_builtins() {
    let custom = vec![custom_commands::CustomCommand {
        name: "review".to_string(),
        description: "review a diff".to_string(),
        path: std::path::PathBuf::from("review.md"),
    }];
    let items = slash_hint_items("/", &custom);
    assert_eq!(items.len(), SLASH_COMMANDS.len() + 1);
    assert_eq!(items.last().unwrap().0, "/review");
    assert_eq!(items.last().unwrap().1, "review a diff");
}

#[test]
fn slash_hint_items_custom_filtered_by_prefix() {
    let custom = vec![custom_commands::CustomCommand {
        name: "review".to_string(),
        description: "review a diff".to_string(),
        path: std::path::PathBuf::from("review.md"),
    }];
    let items = slash_hint_items("/rev", &custom);
    assert_eq!(
        items,
        vec![("/review".to_string(), "review a diff".to_string())]
    );
}

#[test]
fn handle_key_down_moves_hint_selection_not_scroll() {
    let dir = temp_project_dir();
    let mut s = state();
    s.input = "/".to_string();
    handle_key(&mut s, &dir, KeyCode::Down, KeyModifiers::NONE, &None);
    assert_eq!(s.slash_selected, 1);
    assert_eq!(s.scroll, 0);
}

#[test]
fn handle_key_up_down_clamp_hint_selection() {
    let dir = temp_project_dir();
    let mut s = state();
    s.input = "/".to_string();
    for _ in 0..SLASH_COMMANDS.len() + 2 {
        handle_key(&mut s, &dir, KeyCode::Down, KeyModifiers::NONE, &None);
    }
    assert_eq!(s.slash_selected, SLASH_COMMANDS.len() - 1);
    for _ in 0..SLASH_COMMANDS.len() + 2 {
        handle_key(&mut s, &dir, KeyCode::Up, KeyModifiers::NONE, &None);
    }
    assert_eq!(s.slash_selected, 0);
}

#[test]
fn handle_key_tab_completes_highlighted_command() {
    let dir = temp_project_dir();
    let mut s = state();
    s.input = "/".to_string();
    handle_key(&mut s, &dir, KeyCode::Down, KeyModifiers::NONE, &None);
    handle_key(&mut s, &dir, KeyCode::Tab, KeyModifiers::NONE, &None);
    assert_eq!(s.input, "/effort ");
    assert_eq!(s.slash_selected, 0);
}

#[test]
fn handle_key_typing_resets_hint_selection() {
    let dir = temp_project_dir();
    let mut s = state();
    s.input = "/".to_string();
    s.slash_selected = 2;
    handle_key(&mut s, &dir, KeyCode::Char('m'), KeyModifiers::NONE, &None);
    assert_eq!(s.slash_selected, 0);
    assert_eq!(s.input, "/m");
}

#[test]
fn handle_key_esc_clears_input_when_hint_visible() {
    let dir = temp_project_dir();
    let mut s = state();
    s.input = "/mo".to_string();
    handle_key(&mut s, &dir, KeyCode::Esc, KeyModifiers::NONE, &None);
    assert!(s.input.is_empty());
    assert!(!s.running);
}

#[test]
fn running_esc_requires_second_press_to_cancel() {
    let dir = temp_project_dir();
    let mut s = state();
    s.running = true;
    let cancel = kode_core::CancellationToken::new();
    let current = Some(cancel.clone());

    handle_key(&mut s, &dir, KeyCode::Esc, KeyModifiers::NONE, &current);
    assert!(s.interrupt_confirmation_active());
    assert!(!cancel.is_cancelled());

    handle_key(&mut s, &dir, KeyCode::Esc, KeyModifiers::NONE, &current);
    assert!(cancel.is_cancelled());
    assert!(!s.interrupt_confirmation_active());
}

#[test]
fn expired_or_abandoned_interrupt_confirmation_does_not_cancel() {
    let dir = temp_project_dir();
    let mut s = state();
    s.running = true;
    let cancel = kode_core::CancellationToken::new();
    let current = Some(cancel.clone());

    s.interrupt_armed_at =
        Some(Instant::now() - INTERRUPT_CONFIRM_WINDOW - Duration::from_millis(1));
    handle_key(&mut s, &dir, KeyCode::Esc, KeyModifiers::NONE, &current);
    assert!(s.interrupt_confirmation_active());
    assert!(!cancel.is_cancelled());

    handle_key(
        &mut s,
        &dir,
        KeyCode::Char('x'),
        KeyModifiers::NONE,
        &current,
    );
    assert!(!s.interrupt_confirmation_active());
    assert!(!cancel.is_cancelled());
}

#[test]
fn slash_hint_lines_marks_selected_row() {
    let items: Vec<(String, String)> = vec![
        ("/model".to_string(), "a".to_string()),
        ("/effort".to_string(), "b".to_string()),
    ];
    let lines = slash_hint_lines(&items, 1);
    assert!(!lines[0].spans[0].content.contains('›'));
    assert!(lines[1].spans[0].content.contains('›'));
}
