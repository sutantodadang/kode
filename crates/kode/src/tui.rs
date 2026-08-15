use std::collections::VecDeque;
use std::io::Stdout;
use std::path::Path;
use std::sync::Arc;

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
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use tokio::sync::{mpsc, oneshot};

use crate::pipeline;

/// The agent run's current phase, shown in the status bar.
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
    pub model: String,
    pub context_tokens: usize,
    pub tools_used: u32,
    pub state: RunState,
}

impl StatusInfo {
    fn new(model: String) -> Self {
        Self {
            model,
            context_tokens: 0,
            tools_used: 0,
            state: RunState::Idle,
        }
    }
}

/// A pending permission request awaiting a y/n answer from the user.
pub struct PermReq {
    pub summary: String,
    pub responder: oneshot::Sender<bool>,
}

/// Pure UI state, driven by `apply_event`. Kept free of any terminal I/O so
/// it can be unit tested directly.
pub struct AppState {
    pub transcript: Vec<String>,
    pub current_stream: String,
    pub status: StatusInfo,
    pub running: bool,
    pub pending: VecDeque<PermReq>,
    pub scroll: u16,
    pub follow: bool,
    pub input: String,
}

impl AppState {
    pub fn new(model: String) -> Self {
        Self {
            transcript: Vec::new(),
            current_stream: String::new(),
            status: StatusInfo::new(model),
            running: false,
            pending: VecDeque::new(),
            scroll: 0,
            follow: true,
            input: String::new(),
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
        state
            .transcript
            .push(std::mem::take(&mut state.current_stream));
    }

    match ev {
        KodeEvent::AgentStarted => {
            state.running = true;
            state.status.state = RunState::Thinking;
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
        }
        KodeEvent::ModelToken { text } => {
            state.current_stream.push_str(&text);
        }
        KodeEvent::ToolRequested { .. } => {}
        KodeEvent::ToolStarted { name } => {
            state.transcript.push(format!("▸ {name}"));
            state.status.tools_used += 1;
            state.status.state = RunState::Tool;
        }
        KodeEvent::ToolFinished { name, ok } => {
            if !ok {
                state.transcript.push(format!("▸ {name} failed"));
            }
        }
        KodeEvent::VerificationStarted => {
            state.status.state = RunState::Verify;
        }
        KodeEvent::VerificationFinished { .. } => {}
        KodeEvent::AgentFinished => {
            state.running = false;
            state.status.state = RunState::Idle;
        }
        KodeEvent::AgentError { message } => {
            state.transcript.push(format!("error: {message}"));
            state.running = false;
            state.status.state = RunState::Idle;
        }
        KodeEvent::Note { text } => {
            state.transcript.push(format!("◆ {text}"));
        }
        KodeEvent::TaskFinished {
            iterations,
            tool_calls,
            input_tokens,
            output_tokens,
        } => {
            state.transcript.push(format!(
                "— {iterations} iterations, {tool_calls} tool calls, {input_tokens}→{output_tokens} tokens"
            ));
            state.running = false;
            state.status.state = RunState::Idle;
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

/// Launches the interactive TUI. Runs until the user quits (Ctrl-C/'q' while
/// idle) or the process is otherwise terminated.
pub async fn run(cwd: &Path, cancel: CancellationToken) -> anyhow::Result<()> {
    let config = KodeConfig::load(cwd).unwrap_or_default();

    enable_raw_mode()?;
    execute!(std::io::stdout(), EnterAlternateScreen)?;
    let _guard = TerminalGuard;

    let backend = CrosstermBackend::new(std::io::stdout());
    let mut terminal = Terminal::new(backend)?;

    let mut state = AppState::new(config.model.model.clone());

    let (perm_tx, mut perm_rx) = mpsc::unbounded_channel::<(String, oneshot::Sender<bool>)>();
    let handler: Arc<dyn PermissionHandler> = Arc::new(TuiPermission::new(perm_tx));

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
                        if handle_key(&mut state, key.code, key.modifiers, &current_cancel) {
                            break 'outer;
                        }
                        if key.code == KeyCode::Enter && !state.running && !state.input.trim().is_empty() {
                            let task = std::mem::take(&mut state.input);
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

fn draw(f: &mut ratatui::Frame, state: &AppState) {
    let has_pending = !state.pending.is_empty();
    let constraints = if has_pending {
        vec![
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(3),
        ]
    } else {
        vec![
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(3),
        ]
    };
    let areas = Layout::vertical(constraints).split(f.area());

    let mut lines: Vec<Line> = state
        .transcript
        .iter()
        .map(|l| Line::from(l.as_str()))
        .collect();
    if !state.current_stream.is_empty() {
        lines.push(Line::from(state.current_stream.as_str()));
    }
    let transcript = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .scroll((state.scroll, 0))
        .block(Block::default().borders(Borders::NONE));
    f.render_widget(transcript, areas[0]);

    let status = format!(
        "{} | {} tokens | {} tools | {}",
        if state.status.model.is_empty() {
            "(no model)"
        } else {
            state.status.model.as_str()
        },
        state.status.context_tokens,
        state.status.tools_used,
        state.status.state.label(),
    );
    f.render_widget(Paragraph::new(status), areas[1]);

    let next_area = if has_pending {
        let req_line = state
            .pending
            .front()
            .map(|r| format!("allow: {}  [y]es / [n]o", r.summary))
            .unwrap_or_default();
        f.render_widget(
            Paragraph::new(Span::styled(
                req_line,
                Style::default().bg(Color::Yellow).fg(Color::Black),
            )),
            areas[2],
        );
        areas[3]
    } else {
        areas[2]
    };

    let input = Paragraph::new(state.input.as_str())
        .block(Block::default().borders(Borders::ALL).title("task"));
    f.render_widget(input, next_area);
}

#[allow(dead_code)]
type Backend = CrosstermBackend<Stdout>;

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> AppState {
        AppState::new("gpt-test".to_string())
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
        assert_eq!(s.transcript, vec!["thinking...", "▸ read_file"]);
        assert!(s.current_stream.is_empty());
        assert_eq!(s.status.tools_used, 1);
        assert_eq!(s.status.state, RunState::Tool);
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
        assert_eq!(s.transcript, vec!["▸ run_shell failed"]);
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
    fn note_pushes_diamond_prefixed_line() {
        let mut s = state();
        apply_event(
            &mut s,
            KodeEvent::Note {
                text: "code intelligence unavailable".into(),
            },
        );
        assert_eq!(s.transcript, vec!["◆ code intelligence unavailable"]);
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
            vec!["— 3 iterations, 5 tool calls, 100→50 tokens"]
        );
    }

    #[test]
    fn agent_started_sets_running_and_thinking() {
        let mut s = state();
        apply_event(&mut s, KodeEvent::AgentStarted);
        assert!(s.running);
        assert_eq!(s.status.state, RunState::Thinking);
    }

    #[test]
    fn agent_finished_clears_running() {
        let mut s = state();
        s.running = true;
        apply_event(&mut s, KodeEvent::AgentFinished);
        assert!(!s.running);
        assert_eq!(s.status.state, RunState::Idle);
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
        assert_eq!(s.transcript, vec!["error: boom"]);
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
}
