use std::ops::AddAssign;

use kode_core::ImageAttachment;

#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    System(String),
    User(String),
    UserWithImages {
        content: String,
        images: Vec<ImageAttachment>,
    },
    Assistant {
        content: String,
        tool_calls: Vec<ToolCall>,
    },
    Tool {
        tool_call_id: String,
        content: String,
    },
}

impl Message {
    pub fn user(input: kode_core::UserInput) -> Self {
        if input.images.is_empty() {
            Self::User(input.text)
        } else {
            Self::UserWithImages {
                content: input.text,
                images: input.images,
            }
        }
    }
}

pub(crate) fn image_data_url(image: &ImageAttachment) -> String {
    format!("data:{};base64,{}", image.media_type, image.data)
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
    /// Stable for one Kode process. Lets providers route to a warm prompt
    /// cache. `None` disables every cache hint.
    pub cache_key: Option<String>,
    /// Index into `messages` of the last replayed-history message. Providers
    /// with explicit cache breakpoints mark it; the rest ignore it.
    pub cache_anchor: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Usage {
    /// Total input tokens, cached share included.
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Input tokens served from the provider's prompt cache. `None` means
    /// the provider did not report it.
    pub cache_read_tokens: Option<u64>,
    /// Input tokens written to the cache. Only Anthropic reports this.
    pub cache_write_tokens: Option<u64>,
}

impl Usage {
    pub fn total(&self) -> u64 {
        self.input_tokens + self.output_tokens
    }
}

fn add_reported(a: Option<u64>, b: Option<u64>) -> Option<u64> {
    match (a, b) {
        (None, None) => None,
        (a, b) => Some(a.unwrap_or(0) + b.unwrap_or(0)),
    }
}

impl AddAssign for Usage {
    fn add_assign(&mut self, rhs: Self) {
        self.input_tokens += rhs.input_tokens;
        self.output_tokens += rhs.output_tokens;
        self.cache_read_tokens = add_reported(self.cache_read_tokens, rhs.cache_read_tokens);
        self.cache_write_tokens = add_reported(self.cache_write_tokens, rhs.cache_write_tokens);
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
            ..Default::default()
        };
        let b = Usage {
            input_tokens: 3,
            output_tokens: 7,
            ..Default::default()
        };
        a += b;
        assert_eq!(a.input_tokens, 13);
        assert_eq!(a.output_tokens, 12);
        assert_eq!(a.total(), 25);
        assert_eq!(a.cache_read_tokens, None);
        assert_eq!(a.cache_write_tokens, None);
    }

    #[test]
    fn usage_add_assign_keeps_reported_cache_counts() {
        let mut a = Usage::default();
        a += Usage {
            cache_read_tokens: Some(40),
            ..Default::default()
        };
        assert_eq!(a.cache_read_tokens, Some(40));
        a += Usage::default();
        assert_eq!(a.cache_read_tokens, Some(40));
        a += Usage {
            cache_read_tokens: Some(2),
            cache_write_tokens: Some(9),
            ..Default::default()
        };
        assert_eq!(a.cache_read_tokens, Some(42));
        assert_eq!(a.cache_write_tokens, Some(9));
    }

    #[test]
    fn model_request_defaults_send_no_cache_hints() {
        let request = ModelRequest::default();
        assert_eq!(request.cache_key, None);
        assert_eq!(request.cache_anchor, None);
    }
}
