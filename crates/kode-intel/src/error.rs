use thiserror::Error;

#[derive(Debug, Error)]
pub enum IntelError {
    #[error("code intelligence unavailable: {0}")]
    Unavailable(String),
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("repository not indexed: run `zindeks index .` in {0}")]
    NotIndexed(String),
    #[error("zindeks error: {0}")]
    Tool(String),
    #[error("request timed out")]
    Timeout,
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, IntelError>;

impl From<kode_mcp::McpError> for IntelError {
    fn from(err: kode_mcp::McpError) -> Self {
        match err {
            kode_mcp::McpError::Unavailable(msg) => IntelError::Unavailable(msg),
            kode_mcp::McpError::Protocol(msg) => IntelError::Protocol(msg),
            kode_mcp::McpError::Tool(msg) => IntelError::Tool(msg),
            kode_mcp::McpError::Timeout => IntelError::Timeout,
            kode_mcp::McpError::Io(e) => IntelError::Io(e),
        }
    }
}
