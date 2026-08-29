mod error;
mod prompt_budget;
mod subagent;

pub use error::{AgentError, Result};
pub use subagent::SubagentTool;

use std::sync::Arc;

use futures::StreamExt;
use kode_core::config::AgentConfig;
use kode_core::event::{EventBus, KodeEvent};
use kode_core::{ImageAttachment, UserInput};
use kode_model::{
    Message, Model, ModelError, ModelRequest, ResponseAccumulator, StreamEvent, ToolSpec, Usage,
    collect_response,
};
use kode_tools::registry::ToolRuntime;
use kode_tools::{ToolContext, ToolError};
use prompt_budget::PromptBudget;
use tokio::sync::mpsc;

const MAX_TOOL_LABEL_CHARS: usize = 240;
const MAX_MODEL_RETRY_DELAY_MS: u64 = 30_000;
const COMPACTED_CONTEXT_PREFIX: &str = "Compacted work context:";
const COMPACTION_PROMPT: &str = "You are compacting an active coding-agent conversation so work can continue without re-reading the full transcript. Produce a dense, factual structured summary. Preserve: the user's objective and corrections; decisions and constraints; exact file paths, symbols, commands, edits, and observed results; failed approaches and error text; repository state; current progress; and remaining work. Distinguish completed from pending work. Never invent facts. Omit pleasantries and repeated tool output. The original session remains stored, but this summary must be sufficient to continue correctly.";

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
    if name == "delegate_task" {
        let id = arguments
            .get("id")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("leaf");
        let mode = if arguments
            .get("read_only")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true)
        {
            "read-only"
        } else {
            "scoped write"
        };
        return truncate_tool_label(format!("{name} · {id} · {mode}"));
    }
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

Environment: OS is `{os}`. `run_command` spawns the program directly with NO shell: no pipes, redirects, globs or builtins, and Unix tools such as `rg`, `grep`, `find`, `cat`, `ls`, `sed` are NOT guaranteed to exist (they usually do not on Windows). When `code_search` and `file_outline` are offered, use them first for conceptual, symbol, implementation, and call-site discovery because they query the indexed code graph. Use `git grep` through `run_command` only for exact literal matching or when code-intelligence tools are unavailable. To read exact file contents use `read_file`. Do not retry a program that was reported as not found.

Skills: when repository context lists available skills, call `use_skill` before taking task actions if the user names a skill (for example `$review`) or the task clearly matches a skill description. Read `SKILL.md` first, then use `use_skill` with a relative `path` for any referenced resource you need. User instructions override skill instructions.

Delegation: when `delegate_task` is offered, use it only for an independent, bounded leaf task that materially helps the root task. Give the child complete context and narrow, explicit ownership. Prefer read-only investigation. The root agent owns integration, conflict resolution, final verification, and the final user response. Never delegate the entire task.",
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
    max_tool_calls: u32,
    model_retries: u32,
    model_retry_base_ms: u64,
    prompt_budget: PromptBudget,
    auto_compact: bool,
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
    pub images: Vec<ImageAttachment>,
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
        let cost = (t.task.len() + t.response.len()) / 4 + t.images.len() * 1600;
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
        let agent_cfg = agent_cfg.resolved(None);
        Self {
            model,
            tools,
            events,
            max_tool_calls: agent_cfg.max_tool_calls,
            model_retries: agent_cfg.model_retries.min(10),
            model_retry_base_ms: agent_cfg.model_retry_base_ms,
            prompt_budget: PromptBudget::new(agent_cfg.max_context_tokens),
            auto_compact: agent_cfg.auto_compact,
            effort: None,
        }
    }

    async fn wait_for_model_retry(
        &self,
        retry: u32,
        error: &ModelError,
        ctx: &ToolContext,
    ) -> Result<()> {
        let exponent = retry.saturating_sub(1).min(16);
        let delay_ms = self
            .model_retry_base_ms
            .saturating_mul(1u64 << exponent)
            .min(MAX_MODEL_RETRY_DELAY_MS);
        let reason = truncate_tool_label(error.to_string().replace(['\r', '\n'], " "));
        self.events.emit(KodeEvent::Note {
            text: format!(
                "model temporarily unavailable; retrying {retry}/{} in {delay_ms}ms ({reason})",
                self.model_retries
            ),
        });

        tokio::select! {
            _ = ctx.cancel.cancelled() => Err(AgentError::Cancelled),
            _ = tokio::time::sleep(std::time::Duration::from_millis(delay_ms)) => Ok(()),
        }
    }

    async fn compact_messages(
        &self,
        messages: &mut Vec<Message>,
        tools: &[ToolSpec],
        task: &UserInput,
    ) -> Result<Option<(Usage, usize, usize)>> {
        let before = self.prompt_budget.estimate(messages, tools);
        let mut compact_request = Vec::with_capacity(messages.len() + 1);
        compact_request.push(Message::System(COMPACTION_PROMPT.to_string()));
        compact_request.extend(messages.iter().cloned());
        if self.prompt_budget.estimate(&compact_request, &[]) > self.prompt_budget.input_budget() {
            compact_request = self.prompt_budget.prepare(&compact_request, &[])?;
        }
        let compact_output_tokens = self
            .prompt_budget
            .context_window()
            .saturating_sub(self.prompt_budget.estimate(&compact_request, &[]))
            .clamp(1, 16_384) as u32;

        let stream = self
            .model
            .stream(ModelRequest {
                messages: compact_request,
                tools: Vec::new(),
                max_tokens: Some(compact_output_tokens),
                temperature: None,
                effort: None,
            })
            .await?;
        let response = collect_response(stream).await?;
        let summary = response.content.trim();
        if summary.is_empty() {
            return Ok(None);
        }

        let mut retained = messages
            .iter()
            .filter(|message| {
                matches!(message, Message::System(text) if !text.starts_with(COMPACTED_CONTEXT_PREFIX))
            })
            .cloned()
            .collect::<Vec<_>>();
        retained.push(Message::System(format!(
            "{COMPACTED_CONTEXT_PREFIX}\n{summary}"
        )));

        let task_message = Message::user(task.clone());
        let mut exact_indices = Vec::new();
        if let Some(index) = messages
            .iter()
            .rposition(|message| message == &task_message)
        {
            exact_indices.push(index);
        }
        if let Some(index) = messages.iter().rposition(|message| {
            matches!(message, Message::User(_) | Message::UserWithImages { .. })
        }) {
            exact_indices.push(index);
        }
        if let Some((start, end)) = prompt_budget::completed_tool_rounds(messages)
            .last()
            .copied()
        {
            exact_indices.extend(start..end);
        }
        exact_indices.sort_unstable();
        exact_indices.dedup();
        retained.extend(
            exact_indices
                .into_iter()
                .filter_map(|index| messages.get(index))
                .filter(|message| !matches!(message, Message::System(_)))
                .cloned(),
        );

        *messages = self.prompt_budget.prepare(&retained, tools)?;
        let after = self.prompt_budget.estimate(messages, tools);
        Ok(Some((response.usage.unwrap_or_default(), before, after)))
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
        let task = UserInput::text(task);
        self.run_with_context_and_steering(&task, context, history, truncated, ctx, None)
            .await
    }

    /// Runs an agent turn while accepting additional user messages through
    /// `steering`. Steering is serialized into the same message history at
    /// model/tool boundaries; it never starts a concurrent agent.
    pub async fn run_with_context_and_steering(
        &self,
        task: &UserInput,
        context: Option<&str>,
        history: &[HistoryTurn],
        truncated: bool,
        ctx: &ToolContext,
        mut steering: Option<&mut mpsc::UnboundedReceiver<UserInput>>,
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
            messages.push(Message::user(UserInput {
                text: turn.task.clone(),
                images: turn.images.clone(),
            }));
            messages.push(Message::Assistant {
                content: turn.response.clone(),
                tool_calls: vec![],
            });
        }
        messages.push(Message::user(task.clone()));

        let mut usage = Usage::default();
        let mut total_tool_calls: u32 = 0;
        let mut last_call: Option<(String, String)> = None;
        let mut repeat_count: u32 = 0;
        let mut mutated = false;
        let mut steering_open = steering.is_some();
        let mut compaction_available = self.auto_compact;

        let mut iteration = 0_u32;
        loop {
            iteration = iteration.saturating_add(1);
            if ctx.cancel.is_cancelled() {
                return Err(AgentError::Cancelled);
            }

            if steering_open && let Some(receiver) = steering.as_deref_mut() {
                while let Ok(message) = receiver.try_recv() {
                    if !message.is_empty() {
                        self.events.emit(KodeEvent::SteeringAccepted {
                            message: message.clone(),
                        });
                        messages.push(Message::user(message));
                    }
                }
            }

            let tools = self.tools.specs();
            if compaction_available && self.prompt_budget.should_compact(&messages, &tools) {
                match self.compact_messages(&mut messages, &tools, task).await {
                    Ok(Some((compact_usage, before, after))) => {
                        usage += compact_usage;
                        self.events.emit(KodeEvent::Note {
                            text: format!(
                                "context auto-compacted: {before} → {after} estimated tokens · {} window",
                                self.prompt_budget.context_window()
                            ),
                        });
                    }
                    Ok(None) => {
                        compaction_available = false;
                        self.events.emit(KodeEvent::Note {
                            text: "auto-compact returned an empty summary; using safe truncation"
                                .to_string(),
                        });
                    }
                    Err(error) => {
                        compaction_available = false;
                        self.events.emit(KodeEvent::Note {
                            text: format!(
                                "auto-compact unavailable ({error}); using safe truncation"
                            ),
                        });
                    }
                }
            }

            self.events.emit(KodeEvent::ModelStarted);
            let request_messages = self.prompt_budget.prepare(&messages, &tools)?;
            let model_request = ModelRequest {
                messages: request_messages,
                tools,
                max_tokens: Some(self.prompt_budget.output_tokens()),
                temperature: None,
                effort: self.effort.clone(),
            };
            let mut steers_after_response = Vec::new();
            let mut retry = 0;
            let response = 'model_attempt: loop {
                let mut stream = match self.model.stream(model_request.clone()).await {
                    Ok(stream) => stream,
                    Err(error) if error.is_retryable() && retry < self.model_retries => {
                        retry += 1;
                        self.wait_for_model_retry(retry, &error, ctx).await?;
                        continue;
                    }
                    Err(error) => return Err(AgentError::Model(error)),
                };
                let mut acc = ResponseAccumulator::new();
                let mut saw_model_delta = false;

                loop {
                    let steer = async {
                        if steering_open {
                            match steering.as_deref_mut() {
                                Some(receiver) => receiver.recv().await,
                                None => std::future::pending::<Option<UserInput>>().await,
                            }
                        } else {
                            std::future::pending::<Option<UserInput>>().await
                        }
                    };
                    tokio::select! {
                        biased;
                        _ = ctx.cancel.cancelled() => {
                            return Err(AgentError::Cancelled);
                        }
                        message = steer => {
                            match message {
                                Some(message) if !message.is_empty() => {
                                    self.events.emit(KodeEvent::SteeringAccepted {
                                        message: message.clone(),
                                    });
                                    steers_after_response.push(message);
                                }
                                Some(_) => {}
                                None => steering_open = false,
                            }
                        }
                        item = stream.next() => {
                            match item {
                                Some(Ok(event)) => {
                                    saw_model_delta = true;
                                    if let StreamEvent::TextDelta(text) = &event {
                                        self.events.emit(KodeEvent::ModelToken { text: text.clone() });
                                    }
                                    acc.push(event);
                                }
                                Some(Err(error))
                                    if !saw_model_delta
                                        && error.is_retryable()
                                        && retry < self.model_retries =>
                                {
                                    retry += 1;
                                    self.wait_for_model_retry(retry, &error, ctx).await?;
                                    continue 'model_attempt;
                                }
                                Some(Err(error)) => return Err(AgentError::Model(error)),
                                None => break 'model_attempt acc.finish()?,
                            }
                        }
                    }
                }
            };

            if steering_open && let Some(receiver) = steering.as_deref_mut() {
                while let Ok(message) = receiver.try_recv() {
                    if !message.is_empty() {
                        self.events.emit(KodeEvent::SteeringAccepted {
                            message: message.clone(),
                        });
                        steers_after_response.push(message);
                    }
                }
            }

            usage += response.usage.unwrap_or_default();

            if response.tool_calls.is_empty() {
                if !steers_after_response.is_empty() {
                    messages.push(Message::Assistant {
                        content: response.content,
                        tool_calls: vec![],
                    });
                    messages.extend(steers_after_response.into_iter().map(Message::user));
                    continue;
                }
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
                if self.max_tool_calls > 0 && total_tool_calls >= self.max_tool_calls {
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
                    .execute_with_effect(&call.name, call.arguments.clone(), ctx)
                    .await
                {
                    Ok((out, tool_mutated)) => {
                        if tool_mutated {
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

            messages.extend(steers_after_response.into_iter().map(Message::user));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kode_core::CancellationToken;
    use kode_core::config::PermissionMode;
    use kode_model::{
        FinishReason, MockModel, Model, ModelCapabilities, ModelRequest, ModelStream,
    };
    use kode_tools::permission::{AutoApprove, AutoDeny};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

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

    #[test]
    fn delegate_task_label_shows_child_and_mode() {
        assert_eq!(
            tool_event_label(
                "delegate_task",
                &serde_json::json!({"id": "api_tests", "read_only": false})
            ),
            "delegate_task · api_tests · scoped write"
        );
    }

    #[test]
    fn system_prompt_requires_progressive_skill_loading() {
        let prompt = system_prompt();
        assert!(prompt.contains("call `use_skill`"));
        assert!(prompt.contains("Read `SKILL.md` first"));
        assert!(prompt.contains("User instructions override skill instructions"));
        assert!(prompt.contains("The root agent owns integration"));
    }

    #[test]
    fn system_prompt_prefers_code_graph_and_limits_grep_to_fallback() {
        let prompt = system_prompt();
        assert!(prompt.contains("use them first"));
        assert!(prompt.contains("`code_search`"));
        assert!(prompt.contains("`file_outline`"));
        assert!(prompt.contains("exact literal matching"));
        assert!(prompt.contains("tools are unavailable"));
    }

    struct DelayedSteeringModel {
        calls: AtomicUsize,
        requests: Mutex<Vec<ModelRequest>>,
    }

    struct OverloadedThenSuccessModel {
        calls: AtomicUsize,
    }

    struct InitialFailureModel {
        calls: AtomicUsize,
        status: u16,
        message: &'static str,
        failures: usize,
    }

    #[async_trait::async_trait]
    impl Model for InitialFailureModel {
        async fn stream(&self, _request: ModelRequest) -> kode_model::Result<ModelStream> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            if call < self.failures {
                return Err(kode_model::ModelError::Api {
                    status: self.status,
                    message: self.message.to_string(),
                });
            }
            Ok(Box::pin(futures::stream::iter([
                Ok(StreamEvent::TextDelta("recovered".to_string())),
                Ok(StreamEvent::Finished {
                    reason: FinishReason::Stop,
                    usage: None,
                }),
            ])))
        }

        fn capabilities(&self) -> ModelCapabilities {
            ModelCapabilities {
                id: "initial-failure-test".to_string(),
                supports_tools: true,
                supports_streaming: true,
            }
        }
    }

    struct PartialThenOverloadedModel {
        calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl Model for PartialThenOverloadedModel {
        async fn stream(&self, _request: ModelRequest) -> kode_model::Result<ModelStream> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(Box::pin(futures::stream::iter([
                Ok(StreamEvent::TextDelta("partial".to_string())),
                Err(kode_model::ModelError::Api {
                    status: 503,
                    message: "service unavailable".to_string(),
                }),
            ])))
        }

        fn capabilities(&self) -> ModelCapabilities {
            ModelCapabilities {
                id: "partial-failure-test".to_string(),
                supports_tools: true,
                supports_streaming: true,
            }
        }
    }

    fn retry_test_config() -> AgentConfig {
        AgentConfig {
            model_retry_base_ms: 0,
            ..Default::default()
        }
    }

    #[async_trait::async_trait]
    impl Model for OverloadedThenSuccessModel {
        async fn stream(&self, _request: ModelRequest) -> kode_model::Result<ModelStream> {
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            if call == 0 {
                return Ok(Box::pin(futures::stream::iter([Err(
                    kode_model::ModelError::Api {
                        status: 0,
                        message: "Our servers are currently overloaded. Please try again later."
                            .to_string(),
                    },
                )])));
            }

            Ok(Box::pin(futures::stream::iter([
                Ok(StreamEvent::TextDelta("recovered".to_string())),
                Ok(StreamEvent::Finished {
                    reason: FinishReason::Stop,
                    usage: None,
                }),
            ])))
        }

        fn capabilities(&self) -> ModelCapabilities {
            ModelCapabilities {
                id: "overload-retry-test".to_string(),
                supports_tools: true,
                supports_streaming: true,
            }
        }
    }

    #[tokio::test]
    async fn retries_transient_stream_failure_before_first_delta() {
        let model = Arc::new(OverloadedThenSuccessModel {
            calls: AtomicUsize::new(0),
        });
        let tools = ToolRuntime::builtin_runtime(PermissionMode::Allow, Arc::new(AutoApprove));
        let agent = Agent::new(
            model.clone(),
            tools,
            EventBus::new(64),
            &retry_test_config(),
        );

        let outcome = agent
            .run("continue the task", &ctx(temp_dir()))
            .await
            .unwrap();

        assert_eq!(outcome.final_text, "recovered");
        assert_eq!(model.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn retries_transient_failure_before_stream_opens() {
        let model = Arc::new(InitialFailureModel {
            calls: AtomicUsize::new(0),
            status: 503,
            message: "service unavailable",
            failures: 1,
        });
        let events = EventBus::new(64);
        let mut rx = events.subscribe();
        let tools = ToolRuntime::builtin_runtime(PermissionMode::Allow, Arc::new(AutoApprove));
        let agent = Agent::new(model.clone(), tools, events, &retry_test_config());

        let outcome = agent
            .run("continue the task", &ctx(temp_dir()))
            .await
            .unwrap();

        assert_eq!(outcome.final_text, "recovered");
        assert_eq!(model.calls.load(Ordering::SeqCst), 2);
        assert!(
            (0..rx.len())
                .filter_map(|_| rx.try_recv().ok())
                .any(|event| {
                    matches!(event, KodeEvent::Note { text } if text.contains("retrying 1/3"))
                })
        );
    }

    #[tokio::test]
    async fn does_not_retry_permanent_model_error() {
        let model = Arc::new(InitialFailureModel {
            calls: AtomicUsize::new(0),
            status: 401,
            message: "invalid credentials",
            failures: usize::MAX,
        });
        let tools = ToolRuntime::builtin_runtime(PermissionMode::Allow, Arc::new(AutoApprove));
        let agent = Agent::new(
            model.clone(),
            tools,
            EventBus::new(64),
            &retry_test_config(),
        );

        let error = agent
            .run("continue the task", &ctx(temp_dir()))
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            AgentError::Model(ModelError::Api { status: 401, .. })
        ));
        assert_eq!(model.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn stops_after_configured_transient_retries_are_exhausted() {
        let model = Arc::new(InitialFailureModel {
            calls: AtomicUsize::new(0),
            status: 503,
            message: "service unavailable",
            failures: usize::MAX,
        });
        let tools = ToolRuntime::builtin_runtime(PermissionMode::Allow, Arc::new(AutoApprove));
        let config = AgentConfig {
            model_retries: 2,
            model_retry_base_ms: 0,
            ..Default::default()
        };
        let agent = Agent::new(model.clone(), tools, EventBus::new(64), &config);

        let error = agent
            .run("continue the task", &ctx(temp_dir()))
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            AgentError::Model(ModelError::Api { status: 503, .. })
        ));
        assert_eq!(model.calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn does_not_retry_after_partial_model_output() {
        let model = Arc::new(PartialThenOverloadedModel {
            calls: AtomicUsize::new(0),
        });
        let tools = ToolRuntime::builtin_runtime(PermissionMode::Allow, Arc::new(AutoApprove));
        let agent = Agent::new(
            model.clone(),
            tools,
            EventBus::new(64),
            &retry_test_config(),
        );

        let error = agent
            .run("continue the task", &ctx(temp_dir()))
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            AgentError::Model(ModelError::Api { status: 503, .. })
        ));
        assert_eq!(model.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn cancellation_interrupts_model_retry_backoff() {
        let model = Arc::new(InitialFailureModel {
            calls: AtomicUsize::new(0),
            status: 503,
            message: "service unavailable",
            failures: usize::MAX,
        });
        let tools = ToolRuntime::builtin_runtime(PermissionMode::Allow, Arc::new(AutoApprove));
        let config = AgentConfig {
            model_retry_base_ms: 10_000,
            ..Default::default()
        };
        let agent = Agent::new(model.clone(), tools, EventBus::new(64), &config);
        let tool_ctx = ctx(temp_dir());
        let cancel = tool_ctx.cancel.clone();

        let run = agent.run("continue the task", &tool_ctx);
        let cancel_soon = async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            cancel.cancel();
        };
        let (result, ()) = tokio::join!(run, cancel_soon);

        assert!(matches!(result.unwrap_err(), AgentError::Cancelled));
        assert_eq!(model.calls.load(Ordering::SeqCst), 1);
    }

    #[async_trait::async_trait]
    impl Model for DelayedSteeringModel {
        async fn stream(&self, request: ModelRequest) -> kode_model::Result<ModelStream> {
            self.requests.lock().unwrap().push(request);
            let call = self.calls.fetch_add(1, Ordering::SeqCst);
            let text = if call == 0 { "first" } else { "steered" };
            let stream = futures::stream::once(async move {
                tokio::time::sleep(Duration::from_millis(30)).await;
                Ok(StreamEvent::TextDelta(text.to_string()))
            })
            .chain(futures::stream::iter([Ok(StreamEvent::Finished {
                reason: FinishReason::Stop,
                usage: None,
            })]));
            Ok(Box::pin(stream))
        }

        fn capabilities(&self) -> ModelCapabilities {
            ModelCapabilities {
                id: "delayed-steering-test".to_string(),
                supports_tools: true,
                supports_streaming: true,
            }
        }
    }

    #[tokio::test]
    async fn steering_during_stream_becomes_next_user_message() {
        let model = Arc::new(DelayedSteeringModel {
            calls: AtomicUsize::new(0),
            requests: Mutex::new(Vec::new()),
        });
        let tools = ToolRuntime::builtin_runtime(PermissionMode::Allow, Arc::new(AutoApprove));
        let agent = Agent::new(
            model.clone(),
            tools,
            EventBus::new(64),
            &AgentConfig::default(),
        );
        let (tx, mut rx) = mpsc::unbounded_channel();
        let dir = temp_dir();
        let tool_ctx = ctx(dir);
        let original = UserInput::text("original");
        let run = agent.run_with_context_and_steering(
            &original,
            None,
            &[],
            false,
            &tool_ctx,
            Some(&mut rx),
        );
        let send = async move {
            tokio::time::sleep(Duration::from_millis(5)).await;
            tx.send(UserInput::text("new direction")).unwrap();
        };

        let (outcome, ()) = tokio::join!(run, send);
        assert_eq!(outcome.unwrap().final_text, "steered");

        let requests = model.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(
            requests[1]
                .messages
                .iter()
                .any(|message| matches!(message, Message::User(text) if text == "new direction"))
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
    async fn auto_compact_summarizes_before_model_window_is_exhausted() {
        let dir = temp_dir();
        std::fs::write(dir.join("large.txt"), "important detail\n".repeat(2_500)).unwrap();

        let mock = MockModel::new();
        let mut tool_call = read_file_call(0, "call_1", "large.txt");
        tool_call.push(StreamEvent::Finished {
            reason: FinishReason::ToolCalls,
            usage: Some(Usage {
                input_tokens: 10,
                output_tokens: 5,
            }),
        });
        mock.push_script(tool_call);
        mock.push_script(vec![
            StreamEvent::TextDelta(
                "Objective: inspect large.txt. Observed: important detail repeats. Remaining: report."
                    .to_string(),
            ),
            StreamEvent::Finished {
                reason: FinishReason::Stop,
                usage: Some(Usage {
                    input_tokens: 11_000,
                    output_tokens: 30,
                }),
            },
        ]);
        mock.push_script(vec![
            StreamEvent::TextDelta("done".to_string()),
            StreamEvent::Finished {
                reason: FinishReason::Stop,
                usage: Some(Usage {
                    input_tokens: 3_000,
                    output_tokens: 4,
                }),
            },
        ]);

        let mock = Arc::new(mock);
        let events = EventBus::new(64);
        let mut rx = events.subscribe();
        let tools = ToolRuntime::builtin_runtime(PermissionMode::Allow, Arc::new(AutoApprove));
        let agent_cfg = AgentConfig {
            max_context_tokens: 12_000,
            auto_compact: true,
            ..Default::default()
        };
        let agent = Agent::new(mock.clone(), tools, events, &agent_cfg);

        let outcome = agent.run("inspect large.txt", &ctx(dir)).await.unwrap();
        assert_eq!(outcome.final_text, "done");
        assert_eq!(outcome.usage.input_tokens, 14_010);

        let requests = mock.requests();
        assert_eq!(requests.len(), 3);
        assert!(requests[1].tools.is_empty());
        assert!(matches!(
            &requests[1].messages[0],
            Message::System(text) if text.contains("compacting an active coding-agent conversation")
        ));
        assert!(requests[2].messages.iter().any(|message| {
            matches!(message, Message::System(text) if text.starts_with(COMPACTED_CONTEXT_PREFIX))
        }));
        assert!(requests[2].messages.iter().any(|message| {
            matches!(message, Message::User(text) if text == "inspect large.txt")
        }));
        let mut saw_compaction = false;
        while let Ok(event) = rx.try_recv() {
            saw_compaction |= matches!(
                event,
                KodeEvent::Note { text } if text.contains("context auto-compacted")
            );
        }
        assert!(saw_compaction);
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
    async fn default_agent_continues_past_one_hundred_tool_calls() {
        let dir = temp_dir();
        let mock = MockModel::new();
        for i in 0..101 {
            let mut script = read_file_call(0, "call", &format!("f{i}.txt"));
            script.push(StreamEvent::Finished {
                reason: FinishReason::ToolCalls,
                usage: None,
            });
            mock.push_script(script);
        }
        mock.push_script(vec![
            StreamEvent::TextDelta("finished".to_string()),
            StreamEvent::Finished {
                reason: FinishReason::Stop,
                usage: None,
            },
        ]);

        let tools = ToolRuntime::builtin_runtime(PermissionMode::Allow, Arc::new(AutoApprove));
        let agent = Agent::new(
            Arc::new(mock),
            tools,
            EventBus::new(256),
            &AgentConfig::default(),
        );

        let outcome = agent.run("long task", &ctx(dir)).await.unwrap();
        assert_eq!(outcome.final_text, "finished");
        assert_eq!(outcome.iterations, 102);
        assert_eq!(outcome.tool_calls, 101);
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
                images: Vec::new(),
                response: "b".into(),
            },
            HistoryTurn {
                task: "c".into(),
                images: Vec::new(),
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
                images: Vec::new(),
                response: big.clone(),
            },
            HistoryTurn {
                task: "new".into(),
                images: Vec::new(),
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
            images: Vec::new(),
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
            images: Vec::new(),
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
