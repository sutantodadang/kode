use thiserror::Error;

#[derive(Debug, Error)]
pub enum ToolError {
    #[error("unknown tool: {0}")]
    UnknownTool(String),
    #[error("invalid arguments for {tool}: {message}")]
    InvalidArgs { tool: String, message: String },
    #[error("permission denied: {0}")]
    Denied(String),
    #[error("path escapes workspace: {0}")]
    PathOutsideWorkspace(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("command timed out after {0:?}")]
    Timeout(std::time::Duration),
    #[error("cancelled")]
    Cancelled,
    #[error("{0}")]
    Failed(String),
}

pub type Result<T> = std::result::Result<T, ToolError>;
