use thiserror::Error;

#[derive(Debug, Error)]
pub enum AgentError {
    #[error(transparent)]
    Model(#[from] kode_model::ModelError),
    #[error("iteration limit reached ({0})")]
    IterationLimit(u32),
    #[error("tool call limit reached ({0})")]
    ToolCallLimit(u32),
    #[error("cancelled")]
    Cancelled,
    #[error(
        "prompt exceeds configured context window: estimated {estimated} input tokens, {available} available"
    )]
    ContextWindowExceeded { estimated: usize, available: usize },
}

pub type Result<T> = std::result::Result<T, AgentError>;
