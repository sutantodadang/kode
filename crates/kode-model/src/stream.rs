use std::collections::BTreeMap;

use futures::StreamExt;

use crate::ModelStream;
use crate::error::{ModelError, Result};
use crate::types::{FinishReason, ModelResponse, StreamEvent, ToolCall, Usage};

#[derive(Default)]
struct PendingToolCall {
    id: Option<String>,
    name: Option<String>,
    arguments: String,
}

/// Accumulates `StreamEvent`s into a `ModelResponse`. Lets callers observe
/// deltas (e.g. to emit progress events) while still collecting the final
/// response.
#[derive(Default)]
pub struct ResponseAccumulator {
    content: String,
    tool_calls: BTreeMap<u32, PendingToolCall>,
    finish_reason: Option<FinishReason>,
    usage: Option<Usage>,
}

impl ResponseAccumulator {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, event: StreamEvent) {
        match event {
            StreamEvent::TextDelta(text) => self.content.push_str(&text),
            StreamEvent::ToolCallDelta {
                index,
                id,
                name,
                arguments_delta,
            } => {
                let entry = self.tool_calls.entry(index).or_default();
                if id.is_some() {
                    entry.id = id;
                }
                if name.is_some() {
                    entry.name = name;
                }
                entry.arguments.push_str(&arguments_delta);
            }
            StreamEvent::Finished { reason, usage } => {
                self.finish_reason = Some(reason);
                self.usage = usage;
            }
        }
    }

    pub fn finish(self) -> Result<ModelResponse> {
        let finish_reason = if let Some(reason) = self.finish_reason {
            reason
        } else {
            // No Finished event ever arrived: the stream was cut before the
            // provider sent a finish chunk. If we already collected partial
            // output, this is a dropped connection (retryable by the agent);
            // if nothing arrived at all, the gateway is misbehaving.
            let detail = if !self.content.is_empty() || !self.tool_calls.is_empty() {
                format!(
                    "truncated model stream: ended without finish event after {} content chars and {} pending tool call(s)",
                    self.content.chars().count(),
                    self.tool_calls.len()
                )
            } else {
                "stream ended without finish event".to_string()
            };
            return Err(ModelError::Parse(detail));
        };

        let mut resolved_tool_calls = Vec::with_capacity(self.tool_calls.len());
        for (_, pending) in self.tool_calls {
            let id = pending
                .id
                .ok_or_else(|| ModelError::Parse("tool call missing id".to_string()))?;
            let name = pending
                .name
                .ok_or_else(|| ModelError::Parse("tool call missing name".to_string()))?;
            let arguments = if pending.arguments.is_empty() {
                serde_json::json!({})
            } else {
                serde_json::from_str(&pending.arguments).map_err(|e| {
                    let prefix = if e.is_eof() {
                        // The stream ended before the JSON payload was complete
                        // (output-token limit or dropped connection). This is
                        // transient; see `ModelError::is_truncated`.
                        "truncated tool call arguments JSON"
                    } else {
                        "invalid tool call arguments JSON"
                    };
                    // A length finish means the provider stopped at its
                    // max_tokens — reasoning-heavy models burn most of the
                    // budget on hidden thinking before the tool call. The
                    // agent escalates the budget on retry when it sees this.
                    let budget_note = if e.is_eof() && matches!(finish_reason, FinishReason::Length)
                    {
                        " (output-token budget exhausted)"
                    } else {
                        ""
                    };
                    ModelError::Parse(format!(
                        "{prefix} for {name} ({} bytes): {e}{budget_note}",
                        pending.arguments.len()
                    ))
                })?
            };
            resolved_tool_calls.push(ToolCall {
                id,
                name,
                arguments,
            });
        }

        Ok(ModelResponse {
            content: self.content,
            tool_calls: resolved_tool_calls,
            finish_reason,
            usage: self.usage,
        })
    }
}

pub async fn collect_response(mut stream: ModelStream) -> Result<ModelResponse> {
    let mut acc = ResponseAccumulator::new();
    while let Some(event) = stream.next().await {
        acc.push(event?);
    }
    acc.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn boxed(events: Vec<StreamEvent>) -> ModelStream {
        Box::pin(futures::stream::iter(events.into_iter().map(Ok)))
    }

    #[tokio::test]
    async fn collects_text_only_response() {
        let events = vec![
            StreamEvent::TextDelta("Hello, ".to_string()),
            StreamEvent::TextDelta("world!".to_string()),
            StreamEvent::Finished {
                reason: FinishReason::Stop,
                usage: Some(Usage {
                    input_tokens: 10,
                    output_tokens: 2,

                    ..Default::default()
                }),
            },
        ];
        let resp = collect_response(boxed(events)).await.unwrap();
        assert_eq!(resp.content, "Hello, world!");
        assert!(resp.tool_calls.is_empty());
        assert_eq!(resp.finish_reason, FinishReason::Stop);
        assert_eq!(
            resp.usage,
            Some(Usage {
                input_tokens: 10,
                output_tokens: 2,

                ..Default::default()
            })
        );
    }

    #[tokio::test]
    async fn collects_split_tool_calls_in_order() {
        let events = vec![
            StreamEvent::ToolCallDelta {
                index: 1,
                id: Some("call_b".to_string()),
                name: Some("second".to_string()),
                arguments_delta: "{\"x\":".to_string(),
            },
            StreamEvent::ToolCallDelta {
                index: 0,
                id: Some("call_a".to_string()),
                name: Some("first".to_string()),
                arguments_delta: "{\"a\":".to_string(),
            },
            StreamEvent::ToolCallDelta {
                index: 1,
                id: None,
                name: None,
                arguments_delta: "2,".to_string(),
            },
            StreamEvent::ToolCallDelta {
                index: 0,
                id: None,
                name: None,
                arguments_delta: "1,".to_string(),
            },
            StreamEvent::ToolCallDelta {
                index: 1,
                id: None,
                name: None,
                arguments_delta: "\"y\":3}".to_string(),
            },
            StreamEvent::ToolCallDelta {
                index: 0,
                id: None,
                name: None,
                arguments_delta: "\"b\":2}".to_string(),
            },
            StreamEvent::Finished {
                reason: FinishReason::ToolCalls,
                usage: None,
            },
        ];
        let resp = collect_response(boxed(events)).await.unwrap();
        assert_eq!(resp.finish_reason, FinishReason::ToolCalls);
        assert_eq!(resp.tool_calls.len(), 2);
        assert_eq!(resp.tool_calls[0].id, "call_a");
        assert_eq!(resp.tool_calls[0].name, "first");
        assert_eq!(
            resp.tool_calls[0].arguments,
            serde_json::json!({"a": 1, "b": 2})
        );
        assert_eq!(resp.tool_calls[1].id, "call_b");
        assert_eq!(resp.tool_calls[1].name, "second");
        assert_eq!(
            resp.tool_calls[1].arguments,
            serde_json::json!({"x": 2, "y": 3})
        );
    }

    #[tokio::test]
    async fn errors_when_no_finish_event() {
        let events = vec![StreamEvent::TextDelta("partial".to_string())];
        let err = collect_response(boxed(events)).await.unwrap_err();
        assert!(matches!(err, ModelError::Parse(_)));
    }

    #[tokio::test]
    async fn errors_on_invalid_tool_call_json() {
        let events = vec![
            StreamEvent::ToolCallDelta {
                index: 0,
                id: Some("call_a".to_string()),
                name: Some("first".to_string()),
                arguments_delta: "{not valid json".to_string(),
            },
            StreamEvent::Finished {
                reason: FinishReason::ToolCalls,
                usage: None,
            },
        ];
        let err = collect_response(boxed(events)).await.unwrap_err();
        assert!(matches!(err, ModelError::Parse(_)));
        assert!(!err.is_truncated());
    }

    #[tokio::test]
    async fn flags_truncated_tool_call_json_as_retryable_by_agent() {
        let events = vec![
            StreamEvent::ToolCallDelta {
                index: 0,
                id: Some("call_a".to_string()),
                name: Some("apply_patch".to_string()),
                arguments_delta: "{\"patch\": \"+248 lines cut off".to_string(),
            },
            StreamEvent::Finished {
                reason: FinishReason::Length,
                usage: None,
            },
        ];
        let err = collect_response(boxed(events)).await.unwrap_err();
        assert!(matches!(err, ModelError::Parse(_)));
        assert!(err.is_truncated());
        let message = err.to_string();
        assert!(message.contains("apply_patch"));
        assert!(message.contains("29 bytes"));
        // Length finish + truncated args => the agent should escalate the
        // output budget on retry, not regenerate an identical request.
        assert!(message.contains("output-token budget exhausted"));
    }

    #[tokio::test]
    async fn stream_drop_truncation_has_no_budget_marker() {
        let events = vec![
            StreamEvent::ToolCallDelta {
                index: 0,
                id: Some("call_a".to_string()),
                name: Some("apply_patch".to_string()),
                arguments_delta: "{\"patch\": \"cut".to_string(),
            },
            StreamEvent::Finished {
                reason: FinishReason::ToolCalls,
                usage: None,
            },
        ];
        let err = collect_response(boxed(events)).await.unwrap_err();
        assert!(err.is_truncated());
        assert!(!err.to_string().contains("budget exhausted"));
    }

    #[tokio::test]
    async fn flags_dropped_stream_without_finish_event_as_truncated() {
        let events = vec![
            StreamEvent::TextDelta("partial answer befor".to_string()),
            StreamEvent::ToolCallDelta {
                index: 0,
                id: Some("call_a".to_string()),
                name: Some("apply_patch".to_string()),
                arguments_delta: "{\"path\": \"src".to_string(),
            },
        ];
        let err = collect_response(boxed(events)).await.unwrap_err();
        assert!(matches!(err, ModelError::Parse(_)));
        assert!(err.is_truncated());
        let message = err.to_string();
        assert!(message.contains("truncated model stream"));
        assert!(message.contains("1 pending tool call"));
    }

    #[tokio::test]
    async fn empty_stream_without_finish_event_stays_non_truncated() {
        let err = collect_response(boxed(vec![])).await.unwrap_err();
        assert!(matches!(err, ModelError::Parse(_)));
        assert!(!err.is_truncated());
        assert_eq!(
            err.to_string(),
            "failed to parse model response: stream ended without finish event"
        );
    }
}
