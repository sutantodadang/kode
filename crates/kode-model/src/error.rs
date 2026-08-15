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

pub type Result<T> = std::result::Result<T, ModelError>;
