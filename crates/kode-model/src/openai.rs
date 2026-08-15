use std::collections::VecDeque;
use std::pin::Pin;

use futures::{Stream, StreamExt};

use crate::error::{ModelError, Result};
use crate::sse::extract_data_lines;
use crate::types::{FinishReason, Message, ModelCapabilities, ModelRequest, Usage};
use crate::{Model, ModelStream};

pub struct OpenAiOptions {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
}

impl Default for OpenAiOptions {
    fn default() -> Self {
        Self {
            base_url: "https://api.openai.com/v1".to_string(),
            api_key: String::new(),
            model: String::new(),
        }
    }
}

// Manual Debug: never expose the api key via logging/Debug-printing.
impl std::fmt::Debug for OpenAiOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiOptions")
            .field("base_url", &self.base_url)
            .field("api_key", &"***redacted***")
            .field("model", &self.model)
            .finish()
    }
}

#[derive(Debug)]
pub struct OpenAiModel {
    client: reqwest::Client,
    opts: OpenAiOptions,
}

impl OpenAiModel {
    pub fn new(opts: OpenAiOptions) -> Self {
        Self {
            client: reqwest::Client::new(),
            opts,
        }
    }
}

#[async_trait::async_trait]
impl Model for OpenAiModel {
    async fn stream(&self, request: ModelRequest) -> Result<ModelStream> {
        let body = build_body(&self.opts.model, &request);
        let url = format!(
            "{}/chat/completions",
            self.opts.base_url.trim_end_matches('/')
        );
        let resp = self
            .client
            .post(url)
            .bearer_auth(&self.opts.api_key)
            .json(&body)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let text = resp.text().await.unwrap_or_default();
            let message: String = text.chars().take(2000).collect();
            return Err(ModelError::Api { status, message });
        }

        let bytes_stream = resp.bytes_stream().map(|r| r.map(|b| b.to_vec()));
        let state = SseState {
            bytes: Box::pin(bytes_stream),
            buffer: String::new(),
            pending: VecDeque::new(),
            held_finish: None,
            last_usage: None,
            done: false,
        };

        let stream = futures::stream::try_unfold(state, sse_step);
        Ok(Box::pin(stream))
    }

    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities {
            id: self.opts.model.clone(),
            supports_tools: true,
            supports_streaming: true,
        }
    }
}

type ByteStream = Pin<Box<dyn Stream<Item = std::result::Result<Vec<u8>, reqwest::Error>> + Send>>;

struct SseState {
    bytes: ByteStream,
    buffer: String,
    pending: VecDeque<crate::types::StreamEvent>,
    held_finish: Option<FinishReason>,
    last_usage: Option<Usage>,
    done: bool,
}

// OpenAI quirk: with stream_options.include_usage, usage arrives in a final
// chunk (empty choices) AFTER the chunk that carries finish_reason. We hold
// the finish reason until the stream truly ends ([DONE] or close) so the
// Finished event carries the latest usage seen.
async fn sse_step(
    mut state: SseState,
) -> std::result::Result<Option<(crate::types::StreamEvent, SseState)>, ModelError> {
    loop {
        if let Some(event) = state.pending.pop_front() {
            return Ok(Some((event, state)));
        }
        if state.done {
            if let Some(reason) = state.held_finish.take() {
                let usage = state.last_usage.take();
                return Ok(Some((
                    crate::types::StreamEvent::Finished { reason, usage },
                    state,
                )));
            }
            return Ok(None);
        }
        match state.bytes.next().await {
            Some(Ok(chunk)) => {
                let text = String::from_utf8_lossy(&chunk).into_owned();
                let lines = extract_data_lines(&mut state.buffer, &text);
                for payload in lines {
                    if payload == "[DONE]" {
                        state.done = true;
                        break;
                    }
                    let wire: ChunkWire = serde_json::from_str(&payload)
                        .map_err(|e| ModelError::Parse(format!("invalid SSE chunk JSON: {e}")))?;
                    let mapped = map_chunk(wire);
                    state.pending.extend(mapped.events);
                    if mapped.finish_reason.is_some() {
                        state.held_finish = mapped.finish_reason;
                    }
                    if mapped.usage.is_some() {
                        state.last_usage = mapped.usage;
                    }
                }
            }
            Some(Err(e)) => return Err(ModelError::Http(e)),
            None => state.done = true,
        }
    }
}

struct MappedChunk {
    events: Vec<crate::types::StreamEvent>,
    finish_reason: Option<FinishReason>,
    usage: Option<Usage>,
}

fn map_chunk(chunk: ChunkWire) -> MappedChunk {
    let mut events = Vec::new();
    let mut finish_reason = None;
    for choice in &chunk.choices {
        if let Some(content) = &choice.delta.content
            && !content.is_empty()
        {
            events.push(crate::types::StreamEvent::TextDelta(content.clone()));
        }
        if let Some(tool_calls) = &choice.delta.tool_calls {
            for tc in tool_calls {
                events.push(crate::types::StreamEvent::ToolCallDelta {
                    index: tc.index,
                    id: tc.id.clone(),
                    name: tc.function.as_ref().and_then(|f| f.name.clone()),
                    arguments_delta: tc
                        .function
                        .as_ref()
                        .and_then(|f| f.arguments.clone())
                        .unwrap_or_default(),
                });
            }
        }
        if let Some(fr) = &choice.finish_reason {
            finish_reason = Some(map_finish_reason(fr));
        }
    }
    let usage = chunk.usage.map(|u| Usage {
        input_tokens: u.prompt_tokens,
        output_tokens: u.completion_tokens,
    });
    MappedChunk {
        events,
        finish_reason,
        usage,
    }
}

fn map_finish_reason(raw: &str) -> FinishReason {
    match raw {
        "stop" => FinishReason::Stop,
        "tool_calls" => FinishReason::ToolCalls,
        "length" => FinishReason::Length,
        other => FinishReason::Other(other.to_string()),
    }
}

#[derive(Debug, serde::Deserialize)]
struct ChunkWire {
    #[serde(default)]
    choices: Vec<ChoiceWire>,
    #[serde(default)]
    usage: Option<UsageWire>,
}

#[derive(Debug, serde::Deserialize)]
struct ChoiceWire {
    #[serde(default)]
    delta: DeltaWire,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Default, serde::Deserialize)]
struct DeltaWire {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<ToolCallDeltaWire>>,
}

#[derive(Debug, serde::Deserialize)]
struct ToolCallDeltaWire {
    index: u32,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<FunctionDeltaWire>,
}

#[derive(Debug, serde::Deserialize)]
struct FunctionDeltaWire {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
struct UsageWire {
    prompt_tokens: u64,
    completion_tokens: u64,
}

fn build_body(model: &str, request: &ModelRequest) -> serde_json::Value {
    let messages: Vec<serde_json::Value> = request.messages.iter().map(message_to_wire).collect();
    let mut body = serde_json::json!({
        "model": model,
        "messages": messages,
        "stream": true,
        "stream_options": {"include_usage": true},
    });
    if !request.tools.is_empty() {
        let tools: Vec<serde_json::Value> = request
            .tools
            .iter()
            .map(|t| {
                serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.parameters,
                    }
                })
            })
            .collect();
        body["tools"] = serde_json::Value::Array(tools);
    }
    if let Some(max_tokens) = request.max_tokens {
        body["max_tokens"] = serde_json::json!(max_tokens);
    }
    if let Some(temperature) = request.temperature {
        body["temperature"] = serde_json::json!(temperature);
    }
    if let Some(effort) = &request.effort {
        body["reasoning_effort"] = serde_json::json!(effort);
    }
    body
}

fn message_to_wire(message: &Message) -> serde_json::Value {
    match message {
        Message::System(content) => serde_json::json!({"role": "system", "content": content}),
        Message::User(content) => serde_json::json!({"role": "user", "content": content}),
        Message::Assistant {
            content,
            tool_calls,
        } => {
            let mut v = serde_json::json!({"role": "assistant", "content": content});
            if !tool_calls.is_empty() {
                let calls: Vec<serde_json::Value> = tool_calls
                    .iter()
                    .map(|tc| {
                        serde_json::json!({
                            "id": tc.id,
                            "type": "function",
                            "function": {
                                "name": tc.name,
                                "arguments": serde_json::to_string(&tc.arguments).unwrap_or_default(),
                            }
                        })
                    })
                    .collect();
                v["tool_calls"] = serde_json::Value::Array(calls);
            }
            v
        }
        Message::Tool {
            tool_call_id,
            content,
        } => serde_json::json!({
            "role": "tool",
            "tool_call_id": tool_call_id,
            "content": content,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ToolCall, ToolSpec};

    #[test]
    fn build_body_full_round_trip() {
        let request = ModelRequest {
            messages: vec![
                Message::System("sys".to_string()),
                Message::User("hi".to_string()),
                Message::Assistant {
                    content: "assist text".to_string(),
                    tool_calls: vec![ToolCall {
                        id: "call_1".to_string(),
                        name: "foo".to_string(),
                        arguments: serde_json::json!({"a": 1}),
                    }],
                },
                Message::Tool {
                    tool_call_id: "call_1".to_string(),
                    content: "tool result".to_string(),
                },
            ],
            tools: vec![ToolSpec {
                name: "foo".to_string(),
                description: "desc".to_string(),
                parameters: serde_json::json!({"type": "object"}),
            }],
            max_tokens: Some(100),
            temperature: Some(0.5),
            effort: None,
        };

        let body = build_body("gpt-4o-mini", &request);

        let expected = serde_json::json!({
            "model": "gpt-4o-mini",
            "messages": [
                {"role": "system", "content": "sys"},
                {"role": "user", "content": "hi"},
                {
                    "role": "assistant",
                    "content": "assist text",
                    "tool_calls": [
                        {
                            "id": "call_1",
                            "type": "function",
                            "function": {"name": "foo", "arguments": "{\"a\":1}"}
                        }
                    ]
                },
                {"role": "tool", "tool_call_id": "call_1", "content": "tool result"}
            ],
            "stream": true,
            "stream_options": {"include_usage": true},
            "tools": [
                {
                    "type": "function",
                    "function": {"name": "foo", "description": "desc", "parameters": {"type": "object"}}
                }
            ],
            "max_tokens": 100,
            "temperature": 0.5
        });

        assert_eq!(body, expected);
    }

    #[test]
    fn build_body_omits_absent_options() {
        let request = ModelRequest {
            messages: vec![Message::User("hi".to_string())],
            tools: vec![],
            max_tokens: None,
            temperature: None,
            effort: None,
        };

        let body = build_body("gpt-4o-mini", &request);

        assert!(body.get("tools").is_none());
        assert!(body.get("max_tokens").is_none());
        assert!(body.get("temperature").is_none());
    }

    #[test]
    fn build_body_includes_reasoning_effort_when_set() {
        let request = ModelRequest {
            messages: vec![Message::User("hi".to_string())],
            tools: vec![],
            max_tokens: None,
            temperature: None,
            effort: Some("low".to_string()),
        };

        let body = build_body("gpt-5", &request);
        assert_eq!(body["reasoning_effort"], serde_json::json!("low"));
    }

    #[test]
    fn build_body_omits_reasoning_effort_when_absent() {
        let request = ModelRequest {
            messages: vec![Message::User("hi".to_string())],
            tools: vec![],
            max_tokens: None,
            temperature: None,
            effort: None,
        };

        let body = build_body("gpt-5", &request);
        assert!(body.get("reasoning_effort").is_none());
    }

    #[test]
    fn map_chunk_content_delta_yields_text_delta() {
        let wire: ChunkWire = serde_json::from_str(
            r#"{"choices":[{"delta":{"content":"hello"},"finish_reason":null}]}"#,
        )
        .unwrap();
        let mapped = map_chunk(wire);
        assert_eq!(
            mapped.events,
            vec![crate::types::StreamEvent::TextDelta("hello".to_string())]
        );
        assert!(mapped.finish_reason.is_none());
        assert!(mapped.usage.is_none());
    }

    #[test]
    fn map_chunk_tool_call_delta_yields_tool_call_delta() {
        let wire: ChunkWire = serde_json::from_str(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"foo","arguments":"{\"a\":"}}]},"finish_reason":null}]}"#,
        )
        .unwrap();
        let mapped = map_chunk(wire);
        assert_eq!(
            mapped.events,
            vec![crate::types::StreamEvent::ToolCallDelta {
                index: 0,
                id: Some("call_1".to_string()),
                name: Some("foo".to_string()),
                arguments_delta: "{\"a\":".to_string(),
            }]
        );
    }

    #[test]
    fn map_chunk_finish_reason_and_usage() {
        let finish_wire: ChunkWire =
            serde_json::from_str(r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#)
                .unwrap();
        let mapped = map_chunk(finish_wire);
        assert_eq!(mapped.finish_reason, Some(FinishReason::ToolCalls));
        assert!(mapped.events.is_empty());

        let usage_wire: ChunkWire = serde_json::from_str(
            r#"{"choices":[],"usage":{"prompt_tokens":5,"completion_tokens":7}}"#,
        )
        .unwrap();
        let mapped = map_chunk(usage_wire);
        assert!(mapped.finish_reason.is_none());
        assert_eq!(
            mapped.usage,
            Some(Usage {
                input_tokens: 5,
                output_tokens: 7
            })
        );
    }
}
