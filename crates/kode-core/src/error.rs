use std::path::PathBuf;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum KodeError {
    #[error("configuration error in {path}: {message}")]
    Config { path: PathBuf, message: String },
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("operation cancelled")]
    Cancelled,
}

pub type Result<T> = std::result::Result<T, KodeError>;
