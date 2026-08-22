mod error;
mod prompt_budget;

pub use error::{AgentError, Result};

use std::sync::Arc;

use futures::StreamExt;
use kode_core::config::AgentConfig;
use kode_core::event::{EventBus, KodeEvent};
use kode_model::{Message, Model, ModelRequest, ResponseAccumulator, StreamEvent, Usage};
use kode_tools::registry::ToolRuntime;
use kode_tools::{RequiredPermission, ToolContext, ToolError};
use prompt_budget::PromptBudget;

const MAX_TOOL_LABEL_CHARS: usize = 240;

fn display_arg(arg: &str) -> String {
    if !arg.is_empty()
        && arg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_-./\\:=@".contains(c))
    {
        arg.to_string()
    } else {
        serde_json::to_string(arg).unwrap_or_else(|_| "\"?\"".to_string())
    }
}

fn truncate_tool_label(label: String) -> String {
    if label.chars().count() <= MAX_TOOL_LABEL_CHARS {
        return label;
    }
    let mut clipped: String = label.chars().take(MAX_TOOL_LABEL_CHARS - 1).collect();
    clipped.push('…');
    clipped
}

fn tool_event_label(name: &str, arguments: &serde_json::Value) -> String {
    if name != "run_command" {
        return name.to_string();
    }

    let Some(program) = arguments.get("program").and_then(serde_json::Value::as_str) else {
        return name.to_string();
    };
    let mut command = display_arg(program);
    if let Some(args) = arguments.get("args").and_then(serde_json::Value::as_array) {
        for arg in args.iter().filter_map(serde_json::Value::as_str) {
            command.push(' ');
            command.push_str(&display_arg(arg));
        }
    }

    let mut label = format!("{name} · {command}");
    if let Some(cwd) = arguments.get("cwd").and_then(serde_json::Value::as_str) {
        label.push_str(" · cwd ");
        label.push_str(cwd);
    }
    if let Some(timeout) = arguments
        .get("timeout_secs")
        .and_then(serde_json::Value::as_u64)
    {
        label.push_str(&format!(" · timeout {timeout}s"));
    }
    truncate_tool_label(label)
}

fn system_prompt() -> String {
    format!(
        "You are Kode, a coding agent operating on the user's repository. Use the provided tools to inspect and modify files and run commands. Prefer reading before writing. When the task is complete, reply with a concise final answer and stop calling tools.

Environment: OS is `{os}`. `run_command` spawns the program directly with NO shell: no pipes, redirects, globs or builtins, and Unix tools such as `rg`, `grep`, `find`, `cat`, `ls`, `sed` are NOT guaranteed to exist (they usually do not on Windows). To search code use `run_command` with program `git` and args like [\"grep\", \"-n\", \"<pattern>\"] — it works on every platform. To read files use `read_file`. Do not retry a program that was reported as not found.",
        os = std::env::consts::OS
    )
}

/// A repeated identical tool call is blocked after this many occurrences in a
/// row, to stop the model from looping on the same no-op action.
const MAX_REPEAT_CALLS: u32 = 2;

pub struct Agent {
    model: Arc<dyn Model>,
    tools: ToolRuntime,
    events: EventBus,
    max_iterations: u32,
    max_tool_calls: u32,
    prompt_budget: PromptBudget,
    effort: Option<String>,
}

#[derive(Debug)]
pub struct AgentOutcome {
    pub final_text: String,
    pub iterations: u32,
    pub tool_calls: u32,
    pub usage: Usage,
    /// True iff at least one tool whose `required_permission()` is
    /// `Mutating` executed successfully during this run.
    pub mutated: bool,
}

/// One prior conversation turn replayed to the model on resume /
/// follow-up tasks. Tool traffic is intentionally absent — see the
/// resume-chat spec (turn-level replay).
#[derive(Debug, Clone, PartialEq)]
pub struct HistoryTurn {
    pub task: String,
    pub response: String,
}

/// Selects the newest suffix of `turns` whose estimated size (chars/4,
/// consistent with kode-context) fits `budget_tokens`. Always keeps at
/// least the newest turn when any exist. Returns the kept slice and
/// whether anything was dropped.
pub fn select_history(turns: &[HistoryTurn], budget_tokens: usize) -> (&[HistoryTurn], bool) {
    let mut start = turns.len();
    let mut used = 0usize;
    while start > 0 {
        let t = &turns[start - 1];
        let cost = (t.task.len() + t.response.len()) / 4;
        if used + cost > budget_tokens && start != turns.len() {
            break;
        }
        used += cost;
        start -= 1;
        if used > budget_tokens {
            break;
        }
    }
    (&turns[start..], start > 0)
}

impl Agent {
    pub fn new(
        model: Arc<dyn Model>,
        tools: ToolRuntime,
        events: EventBus,
        agent_cfg: &AgentConfig,
    ) -> Self {
        Self {
            model,
            tools,
            events,
            max_iterations: agent_cfg.max_iterations,
            max_tool_calls: agent_cfg.max_tool_calls,
            prompt_budget: PromptBudget::new(agent_cfg.max_context_tokens),
            effort: None,
        }
    }

    /// Sets the reasoning-effort hint forwarded on every [`ModelRequest`]
    /// this agent builds. `None` (the default) omits it.
    pub fn with_effort(mut self, effort: Option<String>) -> Self {
        self.effort = effort;
        self
    }

    pub async fn run(&self, task: &str, ctx: &ToolContext) -> Result<AgentOutcome> {
        self.run_with_context(task, None, &[], false, ctx).await
    }

    /// Like [`Self::run`], but with an optional pre-compiled context blob
    /// (e.g. from `kode-context`) injected as a second system message
    /// between the base system prompt and the user's task, and prior
    /// conversation `history` replayed as alternating user/assistant
    /// messages. `history` is an ALREADY-SELECTED slice (see
    /// [`select_history`]) — the caller is responsible for budgeting; when
    /// `truncated` is true a System marker is emitted so the model knows
    /// older turns were dropped.
    pub async fn run_with_context(
        &self,
        task: &str,
        context: Option<&str>,
        history: &[HistoryTurn],
        truncated: bool,
        ctx: &ToolContext,
    ) -> Result<AgentOutcome> {
        self.events.emit(KodeEvent::AgentStarted);

        let mut messages = vec![Message::System(system_prompt())];
        if let Some(c) = context {
            messages.push(Message::System(format!(
                "Repository and session context:\n\n{c}"
            )));
        }
        if truncated {
            messages.push(Message::System(
                "(older conversation truncated)".to_string(),
            ));
        }
        for turn in history {
            messages.push(Message::User(turn.task.clone()));
            messages.push(Message::Assistant {
                content: turn.response.clone(),
                tool_calls: vec![],
            });
        }
        messages.push(Message::User(task.to_string()));

        let mut usage = Usage::default();
        let mut total_tool_calls: u32 = 0;
        let mut last_call: Option<(String, String)> = None;
        let mut repeat_count: u32 = 0;
        let mut mutated = false;

        for iteration in 1..=self.max_iterations {
            if ctx.cancel.is_cancelled() {
                return Err(AgentError::Cancelled);
            }

            self.events.emit(KodeEvent::ModelStarted);
            let tools = self.tools.specs();
            let request_messages = self.prompt_budget.prepare(&messages, &tools)?;
            let mut stream = self
                .model
                .stream(ModelRequest {
                    messages: request_messages,
                    tools,
                    max_tokens: Some(self.prompt_budget.output_tokens()),
                    temperature: None,
                    effort: self.effort.clone(),
                })
                .await?;

            let mut acc = ResponseAccumulator::new();
            let response = loop {
                tokio::select! {
                    biased;
                    _ = ctx.cancel.cancelled() => {
                        return Err(AgentError::Cancelled);
                    }
                    item = stream.next() => {
                        match item {
                            Some(Ok(event)) => {
                                if let StreamEvent::TextDelta(text) = &event {
                                    self.events.emit(KodeEvent::ModelToken { text: text.clone() });
                                }
                                acc.push(event);
                            }
                            Some(Err(e)) => return Err(AgentError::Model(e)),
                            None => break acc.finish()?,
                        }
                    }
                }
            };

            usage += response.usage.unwrap_or_default();

            if response.tool_calls.is_empty() {
                self.events.emit(KodeEvent::AgentFinished);
                return Ok(AgentOutcome {
                    final_text: response.content,
                    iterations: iteration,
                    tool_calls: total_tool_calls,
                    usage,
                    mutated,
                });
            }

            messages.push(Message::Assistant {
                content: response.content,
                tool_calls: response.tool_calls.clone(),
            });

            for call in &response.tool_calls {
                if total_tool_calls >= self.max_tool_calls {
                    return Err(AgentError::ToolCallLimit(self.max_tool_calls));
                }

                let canonical_args = serde_json::to_string(&call.arguments).unwrap_or_default();
                let is_repeat = last_call
                    .as_ref()
                    .is_some_and(|(name, args)| *name == call.name && *args == canonical_args);
                repeat_count = if is_repeat { repeat_count + 1 } else { 1 };
                last_call = Some((call.name.clone(), canonical_args));

                if repeat_count > MAX_REPEAT_CALLS {
                    total_tool_calls += 1;
                    messages.push(Message::Tool {
                        tool_call_id: call.id.clone(),
                        content:
                            "error: identical tool call repeated too many times; change approach"
                                .to_string(),
                    });
                    continue;
                }

                self.events.emit(KodeEvent::ToolRequested {
                    name: call.name.clone(),
                });
                self.events.emit(KodeEvent::ToolStarted {
                    name: tool_event_label(&call.name, &call.arguments),
                });
                total_tool_calls += 1;

                match self
                    .tools
                    .execute(&call.name, call.arguments.clone(), ctx)
                    .await
                {
                    Ok(out) => {
                        if self.tools.required_permission(&call.name)
                            == Some(RequiredPermission::Mutating)
                        {
                            mutated = true;
                        }
                        self.events.emit(KodeEvent::ToolFinished {
                            name: call.name.clone(),
                            ok: true,
                            error: None,
                        });
                        messages.push(Message::Tool {
                            tool_call_id: call.id.clone(),
                            content: out.content,
                        });
                    }
                    Err(ToolError::Cancelled) => {
                        return Err(AgentError::Cancelled);
                    }
                    Err(e) => {
                        self.events.emit(KodeEvent::ToolFinished {
                            name: call.name.clone(),
                            ok: false,
                            error: Some(e.to_string()),
                        });
                        messages.push(Message::Tool {
                            tool_call_id: call.id.clone(),
                            content: format!("error: {e}"),
                        });
                    }
                }
            }
        }

        Err(AgentError::IterationLimit(self.max_iterations))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kode_core::CancellationToken;
    use kode_core::config::PermissionMode;
    use kode_model::{FinishReason, MockModel};
    use kode_tools::permission::{AutoApprove, AutoDeny};
    use std::sync::atomic::{AtomicUsize, Ordering};

    static COUNTER: AtomicUsize = AtomicUsize::new(0);

    fn nanos() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    }

    fn temp_dir() -> std::path::PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "kode-agent-test-{}-{}-{}",
            std::process::id(),
            nanos(),
            n
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn ctx(root: std::path::PathBuf) -> ToolContext {
        ToolContext {
            workspace_root: root,
            cancel: CancellationToken::new(),
        }
    }

    fn read_file_call(index: u32, id: &str, path: &str) -> Vec<StreamEvent> {
        let args = serde_json::json!({ "path": path }).to_string();
        let (first, second) = args.split_at(args.len() / 2);
        vec![
            StreamEvent::ToolCallDelta {
                index,
                id: Some(id.to_string()),
                name: Some("read_file".to_string()),
                arguments_delta: first.to_string(),
            },
            StreamEvent::ToolCallDelta {
                index,
                id: None,
                name: None,
                arguments_delta: second.to_string(),
            },
        ]
    }

    #[test]
    fn run_command_label_shows_exact_invocation_and_controls() {
        let label = tool_event_label(
            "run_command",
            &serde_json::json!({
                "program": "cargo",
                "args": ["test", "-p", "kode agent", ""],
                "cwd": "crates/kode-agent",
                "timeout_secs": 600
            }),
        );

        assert_eq!(
            label,
            "run_command · cargo test -p \"kode agent\" \"\" · cwd crates/kode-agent · timeout 600s"
        );
    }

    #[test]
    fn ordinary_tool_label_stays_compact() {
        assert_eq!(
            tool_event_label("read_file", &serde_json::json!({"path": "large.rs"})),
            "read_file"
        );
    }

    #[tokio::test]
    async fn happy_path_tool_call_then_final_text() {
        let dir = temp_dir();
        std::fs::write(dir.join("a.txt"), "hello world").unwrap();

        let mock = MockModel::new();
        let mut script1 = read_file_call(0, "call_1", "a.txt");
        script1.push(StreamEvent::Finished {
            reason: FinishReason::ToolCalls,
            usage: Some(Usage {
                input_tokens: 10,
                output_tokens: 5,
            }),
        });
        mock.push_script(script1);
        mock.push_script(vec![
            StreamEvent::TextDelta("done".to_string()),
            StreamEvent::Finished {
                reason: FinishReason::Stop,
                usage: Some(Usage {
                    input_tokens: 20,
                    output_tokens: 7,
                }),
            },
        ]);

        let events = EventBus::new(64);
        let mut rx = events.subscribe();

        let tools = ToolRuntime::builtin_runtime(PermissionMode::Allow, Arc::new(AutoApprove));
        let agent = Agent::new(Arc::new(mock), tools, events, &AgentConfig::default());

        let outcome = agent.run("read the file", &ctx(dir)).await.unwrap();

        assert_eq!(outcome.final_text, "done");
        assert_eq!(outcome.iterations, 2);
        assert_eq!(outcome.tool_calls, 1);
        assert!(
            !outcome.mutated,
            "read_file-only run must not report mutated"
        );
        assert_eq!(
            outcome.usage,
            Usage {
                input_tokens: 30,
                output_tokens: 12
            }
        );

        let mut collected = Vec::new();
        while let Ok(e) = rx.try_recv() {
            collected.push(e);
        }
        assert!(
            collected
                .iter()
                .any(|e| matches!(e, KodeEvent::AgentStarted))
        );
        assert!(
            collected
                .iter()
                .any(|e| matches!(e, KodeEvent::ModelStarted))
        );
        assert!(
            collected
                .iter()
                .any(|e| matches!(e, KodeEvent::ToolStarted { name } if name == "read_file"))
        );
        assert!(
            collected
                .iter()
                .any(|e| matches!(e, KodeEvent::ToolFinished { ok: true, .. }))
        );
        assert!(
            collected
                .iter()
                .any(|e| matches!(e, KodeEvent::ModelToken { .. }))
        );
        assert!(
            collected
                .iter()
                .any(|e| matches!(e, KodeEvent::AgentFinished))
        );
    }

    #[tokio::test]
    async fn write_file_call_sets_mutated_flag() {
        let dir = temp_dir();

        let mock = MockModel::new();
        let args = serde_json::json!({"path": "out.txt", "content": "hi"}).to_string();
        mock.push_script(vec![
            StreamEvent::ToolCallDelta {
                index: 0,
                id: Some("call_1".to_string()),
                name: Some("write_file".to_string()),
                arguments_delta: args,
            },
            StreamEvent::Finished {
                reason: FinishReason::ToolCalls,
                usage: None,
            },
        ]);
        mock.push_script(vec![
            StreamEvent::TextDelta("done".to_string()),
            StreamEvent::Finished {
                reason: FinishReason::Stop,
                usage: None,
            },
        ]);

        let events = EventBus::new(64);
        let tools = ToolRuntime::builtin_runtime(PermissionMode::Allow, Arc::new(AutoApprove));
        let agent = Agent::new(Arc::new(mock), tools, events, &AgentConfig::default());

        let outcome = agent.run("write a file", &ctx(dir)).await.unwrap();

        assert_eq!(outcome.final_text, "done");
        assert!(outcome.mutated, "write_file run must report mutated");
    }

    #[tokio::test]
    async fn tool_error_is_fed_back_to_model() {
        let dir = temp_dir();

        let mock = MockModel::new();
        let mut script1 = read_file_call(0, "call_1", "does_not_exist.txt");
        script1.push(StreamEvent::Finished {
            reason: FinishReason::ToolCalls,
            usage: None,
        });
        mock.push_script(script1);
        mock.push_script(vec![
            StreamEvent::TextDelta("done".to_string()),
            StreamEvent::Finished {
                reason: FinishReason::Stop,
                usage: None,
            },
        ]);

        let mock = Arc::new(mock);
        let tools = ToolRuntime::builtin_runtime(PermissionMode::Allow, Arc::new(AutoApprove));
        let agent = Agent::new(
            mock.clone(),
            tools,
            EventBus::new(64),
            &AgentConfig::default(),
        );

        let outcome = agent.run("read", &ctx(dir)).await.unwrap();
        assert_eq!(outcome.final_text, "done");

        let requests = mock.requests();
        let second = &requests[1];
        let has_error_message = second
            .messages
            .iter()
            .any(|m| matches!(m, Message::Tool { content, .. } if content.starts_with("error:")));
        assert!(has_error_message);
    }

    #[tokio::test]
    async fn denied_tool_call_is_fed_back_to_model() {
        let dir = temp_dir();

        let mock = MockModel::new();
        let args = serde_json::json!({"path": "x.txt", "content": "y"}).to_string();
        mock.push_script(vec![
            StreamEvent::ToolCallDelta {
                index: 0,
                id: Some("call_1".to_string()),
                name: Some("write_file".to_string()),
                arguments_delta: args,
            },
            StreamEvent::Finished {
                reason: FinishReason::ToolCalls,
                usage: None,
            },
        ]);
        mock.push_script(vec![
            StreamEvent::TextDelta("done".to_string()),
            StreamEvent::Finished {
                reason: FinishReason::Stop,
                usage: None,
            },
        ]);

        let mock = Arc::new(mock);
        let tools = ToolRuntime::builtin_runtime(PermissionMode::Ask, Arc::new(AutoDeny));
        let agent = Agent::new(
            mock.clone(),
            tools,
            EventBus::new(64),
            &AgentConfig::default(),
        );

        let outcome = agent.run("write", &ctx(dir)).await.unwrap();
        assert_eq!(outcome.final_text, "done");

        let requests = mock.requests();
        let second = &requests[1];
        let tool_message_ok = second.messages.iter().any(|m| {
            matches!(
                m,
                Message::Tool { content, .. }
                    if content.starts_with("error:") && content.contains("denied")
            )
        });
        assert!(tool_message_ok);
    }

    #[tokio::test]
    async fn iteration_limit_returns_err() {
        let dir = temp_dir();
        let mock = MockModel::new();
        for i in 0..3 {
            let mut script = read_file_call(0, "call", &format!("f{i}.txt"));
            script.push(StreamEvent::Finished {
                reason: FinishReason::ToolCalls,
                usage: None,
            });
            mock.push_script(script);
        }

        let tools = ToolRuntime::builtin_runtime(PermissionMode::Allow, Arc::new(AutoApprove));
        let agent_cfg = AgentConfig {
            max_iterations: 3,
            ..Default::default()
        };
        let agent = Agent::new(Arc::new(mock), tools, EventBus::new(64), &agent_cfg);

        let err = agent.run("loop forever", &ctx(dir)).await.unwrap_err();
        assert!(matches!(err, AgentError::IterationLimit(3)));
    }

    #[tokio::test]
    async fn tool_call_limit_returns_err() {
        let dir = temp_dir();
        let mock = MockModel::new();
        for i in 0..3 {
            let mut script = read_file_call(0, "call", &format!("f{i}.txt"));
            script.push(StreamEvent::Finished {
                reason: FinishReason::ToolCalls,
                usage: None,
            });
            mock.push_script(script);
        }

        let tools = ToolRuntime::builtin_runtime(PermissionMode::Allow, Arc::new(AutoApprove));
        let agent_cfg = AgentConfig {
            max_tool_calls: 2,
            ..Default::default()
        };
        let agent = Agent::new(Arc::new(mock), tools, EventBus::new(64), &agent_cfg);

        let err = agent.run("call tools", &ctx(dir)).await.unwrap_err();
        assert!(matches!(err, AgentError::ToolCallLimit(2)));
    }

    #[tokio::test]
    async fn repeated_identical_call_is_blocked_then_agent_finishes() {
        let dir = temp_dir();
        let mock = MockModel::new();
        for _ in 0..4 {
            let mut script = read_file_call(0, "call", "same.txt");
            script.push(StreamEvent::Finished {
                reason: FinishReason::ToolCalls,
                usage: None,
            });
            mock.push_script(script);
        }
        mock.push_script(vec![
            StreamEvent::TextDelta("done".to_string()),
            StreamEvent::Finished {
                reason: FinishReason::Stop,
                usage: None,
            },
        ]);

        let mock = Arc::new(mock);
        let tools = ToolRuntime::builtin_runtime(PermissionMode::Allow, Arc::new(AutoApprove));
        let agent = Agent::new(
            mock.clone(),
            tools,
            EventBus::new(64),
            &AgentConfig::default(),
        );

        let outcome = agent.run("repeat", &ctx(dir)).await.unwrap();
        assert_eq!(outcome.final_text, "done");

        let requests = mock.requests();
        let has_repeat_message = requests.iter().any(|r| {
            r.messages
                .iter()
                .any(|m| matches!(m, Message::Tool { content, .. } if content.contains("repeated")))
        });
        assert!(has_repeat_message);
    }

    #[tokio::test]
    async fn cancelled_before_first_call_returns_err_and_makes_no_request() {
        let dir = temp_dir();
        let mock = MockModel::new();
        let mock = Arc::new(mock);
        let tools = ToolRuntime::builtin_runtime(PermissionMode::Allow, Arc::new(AutoApprove));
        let agent = Agent::new(
            mock.clone(),
            tools,
            EventBus::new(64),
            &AgentConfig::default(),
        );

        let cancel = CancellationToken::new();
        cancel.cancel();
        let ctx = ToolContext {
            workspace_root: dir,
            cancel,
        };

        let err = agent.run("do nothing", &ctx).await.unwrap_err();
        assert!(matches!(err, AgentError::Cancelled));
        assert!(mock.requests().is_empty());
    }

    #[tokio::test]
    async fn with_effort_is_forwarded_on_model_request() {
        let dir = temp_dir();
        let mock = MockModel::new();
        mock.push_script(vec![
            StreamEvent::TextDelta("done".to_string()),
            StreamEvent::Finished {
                reason: FinishReason::Stop,
                usage: None,
            },
        ]);

        let mock = Arc::new(mock);
        let tools = ToolRuntime::builtin_runtime(PermissionMode::Allow, Arc::new(AutoApprove));
        let agent = Agent::new(
            mock.clone(),
            tools,
            EventBus::new(64),
            &AgentConfig::default(),
        )
        .with_effort(Some("high".to_string()));

        let outcome = agent.run("task", &ctx(dir)).await.unwrap();
        assert_eq!(outcome.final_text, "done");

        let requests = mock.requests();
        assert_eq!(requests[0].effort, Some("high".to_string()));
    }

    #[tokio::test]
    async fn without_effort_model_request_has_none() {
        let dir = temp_dir();
        let mock = MockModel::new();
        mock.push_script(vec![
            StreamEvent::TextDelta("done".to_string()),
            StreamEvent::Finished {
                reason: FinishReason::Stop,
                usage: None,
            },
        ]);

        let mock = Arc::new(mock);
        let tools = ToolRuntime::builtin_runtime(PermissionMode::Allow, Arc::new(AutoApprove));
        let agent = Agent::new(
            mock.clone(),
            tools,
            EventBus::new(64),
            &AgentConfig::default(),
        );

        agent.run("task", &ctx(dir)).await.unwrap();

        let requests = mock.requests();
        assert_eq!(requests[0].effort, None);
    }

    #[tokio::test]
    async fn run_with_context_injects_system_message() {
        let dir = temp_dir();
        let mock = MockModel::new();
        mock.push_script(vec![
            StreamEvent::TextDelta("done".to_string()),
            StreamEvent::Finished {
                reason: FinishReason::Stop,
                usage: None,
            },
        ]);

        let mock = Arc::new(mock);
        let tools = ToolRuntime::builtin_runtime(PermissionMode::Allow, Arc::new(AutoApprove));
        let agent = Agent::new(
            mock.clone(),
            tools,
            EventBus::new(64),
            &AgentConfig::default(),
        );

        let outcome = agent
            .run_with_context("do the thing", Some("CTX_MARKER"), &[], false, &ctx(dir))
            .await
            .unwrap();
        assert_eq!(outcome.final_text, "done");

        let requests = mock.requests();
        let first = &requests[0];
        assert!(
            first
                .messages
                .iter()
                .any(|m| matches!(m, Message::System(content) if content.contains("CTX_MARKER")))
        );
    }

    #[test]
    fn select_history_all_fit() {
        let turns = vec![
            HistoryTurn {
                task: "a".into(),
                response: "b".into(),
            },
            HistoryTurn {
                task: "c".into(),
                response: "d".into(),
            },
        ];
        let (kept, truncated) = select_history(&turns, 1000);
        assert_eq!(kept.len(), 2);
        assert!(!truncated);
    }

    #[test]
    fn select_history_drops_oldest_first() {
        let big = "x".repeat(4000); // ~1000 tokens
        let turns = vec![
            HistoryTurn {
                task: big.clone(),
                response: big.clone(),
            },
            HistoryTurn {
                task: "new".into(),
                response: "answer".into(),
            },
        ];
        let (kept, truncated) = select_history(&turns, 100);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].task, "new");
        assert!(truncated);
    }

    #[test]
    fn select_history_single_oversize_turn_still_kept() {
        let big = "x".repeat(40_000);
        let turns = vec![HistoryTurn {
            task: big.clone(),
            response: big,
        }];
        let (kept, truncated) = select_history(&turns, 100);
        assert_eq!(kept.len(), 1);
        assert!(!truncated);
    }

    #[test]
    fn select_history_empty_is_empty() {
        let (kept, truncated) = select_history(&[], 100);
        assert!(kept.is_empty());
        assert!(!truncated);
    }

    #[tokio::test]
    async fn history_turns_replay_between_context_and_task() {
        let dir = temp_dir();
        let mock = MockModel::new();
        mock.push_script(vec![
            StreamEvent::TextDelta("done".to_string()),
            StreamEvent::Finished {
                reason: FinishReason::Stop,
                usage: None,
            },
        ]);

        let mock = Arc::new(mock);
        let tools = ToolRuntime::builtin_runtime(PermissionMode::Allow, Arc::new(AutoApprove));
        let agent = Agent::new(
            mock.clone(),
            tools,
            EventBus::new(64),
            &AgentConfig::default(),
        );

        let history = vec![HistoryTurn {
            task: "t1".into(),
            response: "r1".into(),
        }];

        let outcome = agent
            .run_with_context("t2", Some("CTX"), &history, false, &ctx(dir))
            .await
            .unwrap();
        assert_eq!(outcome.final_text, "done");

        let requests = mock.requests();
        let first = &requests[0];
        assert_eq!(first.messages.len(), 5);
        assert!(matches!(&first.messages[0], Message::System(_)));
        assert!(matches!(&first.messages[1], Message::System(c) if c.contains("CTX")));
        assert!(matches!(&first.messages[2], Message::User(t) if t == "t1"));
        assert!(
            matches!(&first.messages[3], Message::Assistant { content, tool_calls } if content == "r1" && tool_calls.is_empty())
        );
        assert!(matches!(&first.messages[4], Message::User(t) if t == "t2"));
    }

    #[tokio::test]
    async fn history_truncated_marker_is_injected() {
        let dir = temp_dir();
        let mock = MockModel::new();
        mock.push_script(vec![
            StreamEvent::TextDelta("done".to_string()),
            StreamEvent::Finished {
                reason: FinishReason::Stop,
                usage: None,
            },
        ]);

        let mock = Arc::new(mock);
        let tools = ToolRuntime::builtin_runtime(PermissionMode::Allow, Arc::new(AutoApprove));
        let agent = Agent::new(
            mock.clone(),
            tools,
            EventBus::new(64),
            &AgentConfig::default(),
        );

        let outcome = agent
            .run_with_context("t2", None, &[], true, &ctx(dir))
            .await
            .unwrap();
        assert_eq!(outcome.final_text, "done");

        let requests = mock.requests();
        let first = &requests[0];
        assert!(first.messages.iter().any(
            |m| matches!(m, Message::System(c) if c.contains("older conversation truncated"))
        ));
    }
}
