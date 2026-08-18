use thiserror::Error;

#[derive(Debug, Error)]
pub enum MemoryError {
    #[error("engineering memory unavailable: {0}")]
    Unavailable(String),
    #[error("ingat error ({code}): {message}")]
    Service { code: String, message: String },
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("request timed out")]
    Timeout,
    /// The backend doesn't support the requested operation yet — e.g. an
    /// Ingat build older than the `/import` endpoint (404). Distinct from
    /// `Service`/`Protocol` so callers can degrade gracefully instead of
    /// treating it as a hard failure.
    #[error("backend does not support this operation: {0}")]
    Unsupported(String),
}

pub type Result<T> = std::result::Result<T, MemoryError>;

/// Maps a low-level `reqwest::Error` to a [`MemoryError`]: connect failures
/// become `Unavailable` (the Ingat service isn't reachable), timeouts become
/// `Timeout`, everything else becomes `Protocol`.
pub(crate) fn map_reqwest_error(err: reqwest::Error) -> MemoryError {
    if err.is_timeout() {
        MemoryError::Timeout
    } else if err.is_connect() {
        MemoryError::Unavailable(err.to_string())
    } else {
        MemoryError::Protocol(err.to_string())
    }
}
