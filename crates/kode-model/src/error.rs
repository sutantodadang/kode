use thiserror::Error;

#[derive(Debug, Error)]
pub enum ModelError {
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("api error (status {status}): {message}")]
    Api { status: u16, message: String },
    #[error("failed to parse model response: {0}")]
    Parse(String),
    #[error("model request cancelled")]
    Cancelled,
}

impl ModelError {
    /// Whether repeating the same model request is likely to succeed without
    /// changing user input or credentials.
    ///
    /// Status `0` is used by streaming providers for failures delivered inside
    /// an otherwise-successful SSE connection, so it is retryable only when the
    /// provider message explicitly describes transient capacity or transport
    /// pressure. Local auth/config errors also use `0` and must fail fast.
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Http(error) => error.is_timeout() || error.is_connect() || error.is_body(),
            Self::Api { status, message } => {
                matches!(*status, 408 | 409 | 425 | 429 | 500 | 502 | 503 | 504 | 529)
                    || (*status == 0 && is_transient_provider_message(message))
            }
            Self::Parse(_) | Self::Cancelled => false,
        }
    }

    /// Whether the failure is a tool-call argument payload cut short by a
    /// truncated stream (output-token limit or dropped connection). Not part
    /// of [`Self::is_retryable`] because a blind identical retry can hit the
    /// same token limit; callers that retry should treat this as a distinct,
    /// bounded recovery path.
    pub fn is_truncated_tool_call(&self) -> bool {
        matches!(self, Self::Parse(message) if message.starts_with("truncated tool call arguments"))
    }
}

fn is_transient_provider_message(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    [
        "overload",
        "temporarily unavailable",
        "try again later",
        "rate limit",
        "service unavailable",
        "connection reset",
        "timed out",
        "timeout",
    ]
    .iter()
    .any(|needle| message.contains(needle))
}

pub type Result<T> = std::result::Result<T, ModelError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transient_api_failures_are_retryable() {
        for status in [408, 409, 425, 429, 500, 502, 503, 504, 529] {
            assert!(
                ModelError::Api {
                    status,
                    message: "provider unavailable".to_string(),
                }
                .is_retryable()
            );
        }
        assert!(
            ModelError::Api {
                status: 0,
                message: "Our servers are currently overloaded. Please try again later."
                    .to_string(),
            }
            .is_retryable()
        );
    }

    #[test]
    fn permanent_and_local_status_zero_failures_are_not_retryable() {
        for status in [0, 400, 401, 403, 404, 422] {
            assert!(
                !ModelError::Api {
                    status,
                    message: "invalid codex auth file".to_string(),
                }
                .is_retryable()
            );
        }
        assert!(!ModelError::Parse("invalid SSE".to_string()).is_retryable());
        assert!(!ModelError::Cancelled.is_retryable());
    }

    #[test]
    fn truncated_tool_call_parse_errors_are_flagged() {
        let truncated = ModelError::Parse(
            "truncated tool call arguments JSON for apply_patch (33 bytes): EOF".to_string(),
        );
        assert!(truncated.is_truncated_tool_call());
        // Still not blindly retryable via the transient-failure path.
        assert!(!truncated.is_retryable());

        let invalid = ModelError::Parse(
            "invalid tool call arguments JSON for apply_patch (5 bytes): expected value"
                .to_string(),
        );
        assert!(!invalid.is_truncated_tool_call());
    }
}
