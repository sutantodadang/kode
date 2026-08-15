use thiserror::Error;

#[derive(Debug, Error)]
pub enum McpError {
    #[error("mcp server unavailable: {0}")]
    Unavailable(String),
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("tool error: {0}")]
    Tool(String),
    #[error("request timed out")]
    Timeout,
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, McpError>;
