use std::sync::Arc;

use kode_core::config::{AgentConfig, PermissionMode};
use kode_core::event::{EventBus, KodeEvent};
use kode_model::Model;
use kode_tools::path::WriteScope;
use kode_tools::permission::PermissionHandler;
use kode_tools::registry::{ToolRegistry, ToolRuntime};
use kode_tools::{RequiredPermission, Tool, ToolContext, ToolError, ToolOutput};
use serde::Deserialize;
use tokio::sync::Semaphore;

use crate::{Agent, AgentError};

const MIN_RESULT_CHARS: usize = 1_000;
const MAX_RESULT_CHARS: usize = 50_000;

/// Root-only tool that runs one bounded, non-recursive leaf agent.
pub struct SubagentTool {
    model: Arc<dyn Model>,
    child_registry: ToolRegistry,
    agent_config: AgentConfig,
    effort: Option<String>,
    permission_mode: PermissionMode,
    permission_handler: Arc<dyn PermissionHandler>,
    parent_events: EventBus,
    gate: Arc<Semaphore>,
    max_result_chars: usize,
}

impl SubagentTool {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        model: Arc<dyn Model>,
        child_registry: ToolRegistry,
        agent_config: AgentConfig,
        effort: Option<String>,
        permission_mode: PermissionMode,
        permission_handler: Arc<dyn PermissionHandler>,
        parent_events: EventBus,
        max_result_chars: usize,
    ) -> Self {
        Self {
            model,
            child_registry,
            agent_config,
            effort,
            permission_mode,
            permission_handler,
            parent_events,
            gate: Arc::new(Semaphore::new(1)),
            max_result_chars: max_result_chars.clamp(MIN_RESULT_CHARS, MAX_RESULT_CHARS),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DelegateArgs {
    id: String,
    task: String,
    #[serde(default)]
    context: String,
    #[serde(default = "default_read_only")]
    read_only: bool,
    #[serde(default)]
    ownership: Vec<String>,
}

fn default_read_only() -> bool {
    true
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut truncated = text
        .chars()
        .take(max_chars.saturating_sub(1))
        .collect::<String>();
    truncated.push('…');
    truncated
}

fn single_line(text: &str, max_chars: usize) -> String {
    let line = text.lines().next().unwrap_or_default().trim();
    truncate_chars(line, max_chars)
}

#[async_trait::async_trait]
impl Tool for SubagentTool {
    fn name(&self) -> &str {
        "delegate_task"
    }

    fn description(&self) -> &str {
        "Delegate one independent, bounded task to a non-recursive leaf agent. Use read_only=true for investigation. For edits, set read_only=false and declare narrow workspace-relative ownership roots. The root agent remains responsible for integration and final verification."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": {
                    "type": "string",
                    "pattern": "^[A-Za-z0-9_-]{1,64}$",
                    "description": "Stable short identifier for this child task"
                },
                "task": {
                    "type": "string",
                    "description": "Concrete objective with an explicit deliverable"
                },
                "context": {
                    "type": "string",
                    "description": "Relevant facts, constraints, and file pointers the leaf needs"
                },
                "read_only": {
                    "type": "boolean",
                    "default": true,
                    "description": "When true, every mutating child tool is denied"
                },
                "ownership": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Narrow workspace-relative files/directories this child may edit; required when read_only=false"
                }
            },
            "required": ["id", "task"],
            "additionalProperties": false
        })
    }

    fn required_permission(&self) -> RequiredPermission {
        // Delegation itself is not a write. Child mutating tools still pass
        // through the inherited permission policy and scoped runtime.
        RequiredPermission::ReadOnly
    }

    fn output_mutated(&self, output: &ToolOutput) -> bool {
        serde_json::from_str::<serde_json::Value>(&output.content)
            .ok()
            .and_then(|value| value.get("mutated").and_then(serde_json::Value::as_bool))
            .unwrap_or(false)
    }

    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &ToolContext,
    ) -> kode_tools::Result<ToolOutput> {
        if ctx.cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        let args: DelegateArgs =
            serde_json::from_value(args).map_err(|error| ToolError::InvalidArgs {
                tool: self.name().to_string(),
                message: error.to_string(),
            })?;
        if !valid_id(args.id.trim()) {
            return Err(ToolError::InvalidArgs {
                tool: self.name().to_string(),
                message: "id must contain 1-64 ASCII letters, digits, '-' or '_'".to_string(),
            });
        }
        if args.task.trim().is_empty() {
            return Err(ToolError::InvalidArgs {
                tool: self.name().to_string(),
                message: "task must not be empty".to_string(),
            });
        }

        let write_scope = if args.read_only {
            None
        } else {
            Some(WriteScope::new(&ctx.workspace_root, &args.ownership)?)
        };

        let _permit = tokio::select! {
            _ = ctx.cancel.cancelled() => return Err(ToolError::Cancelled),
            permit = self.gate.acquire() => permit.map_err(|_| ToolError::Failed("subagent gate closed".to_string()))?,
        };

        self.parent_events.emit(KodeEvent::SubagentStarted {
            id: args.id.clone(),
            ownership: args.ownership.clone(),
        });

        let mode = if args.read_only {
            PermissionMode::Deny
        } else {
            self.permission_mode
        };
        let mut runtime = ToolRuntime::new(
            self.child_registry.clone(),
            mode,
            self.permission_handler.clone(),
        );
        if let Some(scope) = write_scope {
            runtime = runtime.with_write_scope(scope);
        }

        let leaf_events = EventBus::new(64);
        let mut leaf_event_rx = leaf_events.subscribe();
        let leaf = Agent::new(self.model.clone(), runtime, leaf_events, &self.agent_config)
            .with_effort(self.effort.clone());
        let leaf_context = format!(
            "You are leaf subagent `{id}`. Complete only the delegated task and return a concise evidence-backed result. You are not the root agent and cannot delegate. Preserve unrelated work in the shared workspace. Mode: {mode}. Declared ownership: {ownership}. Never edit outside ownership.\n\nParent context:\n{context}",
            id = args.id,
            mode = if args.read_only {
                "read-only"
            } else {
                "scoped write"
            },
            ownership = if args.ownership.is_empty() {
                "none".to_string()
            } else {
                args.ownership.join(", ")
            },
            context = args.context.trim(),
        );
        let leaf_ctx = ToolContext {
            workspace_root: ctx.workspace_root.clone(),
            cancel: ctx.cancel.child_token(),
        };

        let child_run =
            leaf.run_with_context(args.task.trim(), Some(&leaf_context), &[], false, &leaf_ctx);
        tokio::pin!(child_run);
        let child_result = loop {
            tokio::select! {
                biased;
                event = leaf_event_rx.recv() => match event {
                    Ok(KodeEvent::ToolStarted { name }) => {
                        self.parent_events.emit(KodeEvent::SubagentActivity {
                            id: args.id.clone(),
                            text: name,
                        });
                    }
                    Ok(KodeEvent::ToolFinished { name, ok: false, error }) => {
                        let reason = error.unwrap_or_else(|| "failed".to_string());
                        self.parent_events.emit(KodeEvent::SubagentActivity {
                            id: args.id.clone(),
                            text: format!("{name} failed: {reason}"),
                        });
                    }
                    Ok(KodeEvent::Note { text }) => {
                        self.parent_events.emit(KodeEvent::SubagentActivity {
                            id: args.id.clone(),
                            text,
                        });
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                        self.parent_events.emit(KodeEvent::SubagentActivity {
                            id: args.id.clone(),
                            text: format!("{skipped} activity events skipped"),
                        });
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {}
                },
                result = &mut child_run => break result,
            }
        };

        match child_result {
            Ok(outcome) => {
                let summary = truncate_chars(outcome.final_text.trim(), self.max_result_chars);
                self.parent_events.emit(KodeEvent::SubagentFinished {
                    id: args.id.clone(),
                    ok: true,
                    summary: single_line(&summary, 240),
                });
                let content = serde_json::to_string(&serde_json::json!({
                    "id": args.id,
                    "status": "completed",
                    "summary": summary,
                    "ownership": args.ownership,
                    "mutated": outcome.mutated,
                    "iterations": outcome.iterations,
                    "tool_calls": outcome.tool_calls,
                    "usage": {
                        "input_tokens": outcome.usage.input_tokens,
                        "output_tokens": outcome.usage.output_tokens,
                    }
                }))
                .map_err(|error| ToolError::Failed(error.to_string()))?;
                Ok(ToolOutput { content })
            }
            Err(AgentError::Cancelled) => {
                self.parent_events.emit(KodeEvent::SubagentFinished {
                    id: args.id,
                    ok: false,
                    summary: "cancelled".to_string(),
                });
                Err(ToolError::Cancelled)
            }
            Err(error) => {
                let summary = single_line(&error.to_string(), 240);
                self.parent_events.emit(KodeEvent::SubagentFinished {
                    id: args.id.clone(),
                    ok: false,
                    summary: summary.clone(),
                });
                let content = serde_json::to_string(&serde_json::json!({
                    "id": args.id,
                    "status": "failed",
                    "error": summary,
                    "ownership": args.ownership,
                    "mutated": false,
                }))
                .map_err(|json_error| ToolError::Failed(json_error.to_string()))?;
                Ok(ToolOutput { content })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kode_model::{FinishReason, MockModel, StreamEvent};
    use kode_tools::permission::AutoApprove;

    fn temp_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kode-subagent-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn finish_script(text: &str) -> Vec<StreamEvent> {
        vec![
            StreamEvent::TextDelta(text.to_string()),
            StreamEvent::Finished {
                reason: FinishReason::Stop,
                usage: None,
            },
        ]
    }

    fn tool(model: Arc<MockModel>, events: EventBus) -> SubagentTool {
        SubagentTool::new(
            model,
            ToolRegistry::with_subagent_builtins(),
            AgentConfig::default(),
            None,
            PermissionMode::Allow,
            Arc::new(AutoApprove),
            events,
            12_000,
        )
    }

    #[tokio::test]
    async fn child_completes_without_recursive_or_command_tools() {
        let model = Arc::new(MockModel::new());
        model.push_script(finish_script("found the answer"));
        let events = EventBus::new(8);
        let mut rx = events.subscribe();
        let output = tool(model.clone(), events)
            .execute(
                serde_json::json!({
                    "id": "investigate",
                    "task": "inspect the implementation"
                }),
                &ToolContext {
                    workspace_root: temp_dir(),
                    cancel: kode_core::CancellationToken::new(),
                },
            )
            .await
            .unwrap();

        assert!(output.content.contains("found the answer"));
        let requests = model.requests();
        let offered = requests[0]
            .tools
            .iter()
            .map(|spec| spec.name.as_str())
            .collect::<Vec<_>>();
        assert!(!offered.contains(&"delegate_task"));
        assert!(!offered.contains(&"run_command"));
        assert!(matches!(
            rx.recv().await.unwrap(),
            KodeEvent::SubagentStarted { .. }
        ));
        assert!(matches!(
            rx.recv().await.unwrap(),
            KodeEvent::SubagentFinished { ok: true, .. }
        ));
    }

    #[tokio::test]
    async fn writable_child_requires_narrow_ownership() {
        let model = Arc::new(MockModel::new());
        let error = tool(model, EventBus::new(8))
            .execute(
                serde_json::json!({
                    "id": "edit",
                    "task": "make a change",
                    "read_only": false,
                    "ownership": []
                }),
                &ToolContext {
                    workspace_root: temp_dir(),
                    cancel: kode_core::CancellationToken::new(),
                },
            )
            .await
            .unwrap_err();

        assert!(matches!(error, ToolError::InvalidArgs { .. }));
    }

    #[tokio::test]
    async fn writable_child_reports_real_mutation_to_parent() {
        let model = Arc::new(MockModel::new());
        let write_args = serde_json::json!({
            "path": "owned/result.txt",
            "content": "from leaf"
        })
        .to_string();
        model.push_script(vec![
            StreamEvent::ToolCallDelta {
                index: 0,
                id: Some("write_1".to_string()),
                name: Some("write_file".to_string()),
                arguments_delta: write_args,
            },
            StreamEvent::Finished {
                reason: FinishReason::ToolCalls,
                usage: None,
            },
        ]);
        model.push_script(finish_script("edit complete"));
        let root = temp_dir();
        std::fs::create_dir_all(root.join("owned")).unwrap();
        let events = EventBus::new(16);
        let mut event_rx = events.subscribe();
        let output = tool(model, events)
            .execute(
                serde_json::json!({
                    "id": "edit",
                    "task": "write the delegated result",
                    "read_only": false,
                    "ownership": ["owned"]
                }),
                &ToolContext {
                    workspace_root: root.clone(),
                    cancel: kode_core::CancellationToken::new(),
                },
            )
            .await
            .unwrap();

        assert_eq!(
            std::fs::read_to_string(root.join("owned/result.txt")).unwrap(),
            "from leaf"
        );
        assert!(output.content.contains(r#""mutated":true"#));
        let mut saw_write_activity = false;
        while let Ok(event) = event_rx.try_recv() {
            if matches!(event, KodeEvent::SubagentActivity { id, text } if id == "edit" && text == "write_file")
            {
                saw_write_activity = true;
            }
        }
        assert!(saw_write_activity);
    }

    #[test]
    fn delegated_mutation_is_reported_dynamically() {
        let model = Arc::new(MockModel::new());
        let tool = tool(model, EventBus::new(8));
        assert!(tool.output_mutated(&ToolOutput {
            content: r#"{"mutated":true}"#.to_string(),
        }));
        assert!(!tool.output_mutated(&ToolOutput {
            content: r#"{"mutated":false}"#.to_string(),
        }));
    }
}
