use std::ops::AddAssign;

#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    System(String),
    User(String),
    Assistant {
        content: String,
        tool_calls: Vec<ToolCall>,
    },
    Tool {
        tool_call_id: String,
        content: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}

/// Tool the model may call. `parameters` is a JSON Schema object.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct ModelRequest {
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
    /// Reasoning-effort hint: "minimal", "low", "medium", "high", "xhigh".
    /// `None` omits it from the wire request entirely.
    pub effort: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

impl Usage {
    pub fn total(&self) -> u64 {
        self.input_tokens + self.output_tokens
    }
}

impl AddAssign for Usage {
    fn add_assign(&mut self, rhs: Self) {
        self.input_tokens += rhs.input_tokens;
        self.output_tokens += rhs.output_tokens;
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum FinishReason {
    Stop,
    ToolCalls,
    Length,
    Other(String),
}

/// Normalized streaming event, provider-agnostic.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    TextDelta(String),
    /// Incremental tool-call fragment. `index` groups fragments of one call.
    ToolCallDelta {
        index: u32,
        id: Option<String>,
        name: Option<String>,
        arguments_delta: String,
    },
    Finished {
        reason: FinishReason,
        usage: Option<Usage>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct ModelResponse {
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
    pub finish_reason: FinishReason,
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ModelCapabilities {
    pub id: String,
    pub supports_tools: bool,
    pub supports_streaming: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_add_assign_accumulates() {
        let mut a = Usage {
            input_tokens: 10,
            output_tokens: 5,
        };
        let b = Usage {
            input_tokens: 3,
            output_tokens: 7,
        };
        a += b;
        assert_eq!(a.input_tokens, 13);
        assert_eq!(a.output_tokens, 12);
        assert_eq!(a.total(), 25);
    }
}
