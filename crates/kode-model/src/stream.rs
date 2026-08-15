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
        let finish_reason = self
            .finish_reason
            .ok_or_else(|| ModelError::Parse("stream ended without finish event".to_string()))?;

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
                    ModelError::Parse(format!("invalid tool call arguments JSON: {e}"))
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
                output_tokens: 2
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
    }
}
