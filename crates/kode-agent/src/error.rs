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
}

pub type Result<T> = std::result::Result<T, AgentError>;
