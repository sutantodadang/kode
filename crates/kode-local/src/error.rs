use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum LocalError {
    #[error("{} is missing — run `kode setup`", .0.display())]
    Missing(PathBuf),
    #[error("checksum mismatch for {}", .0.display())]
    ChecksumMismatch(PathBuf),
    #[error("io error at {}: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("download failed: {0}")]
    Download(String),
    #[error("onnx runtime: {0}")]
    Runtime(String),
    #[error("tokenizer: {0}")]
    Tokenizer(String),
    #[error("invalid model config: {0}")]
    Config(String),
    #[error("question options do not fit the head budget")]
    OptionsDoNotFit,
    #[error("model lock poisoned")]
    Poisoned,
    #[error("this build has no pinned local-model artefacts")]
    NotPinned,
}

/// Maps any displayable runtime error (ort, tokenizers) into `Runtime`.
pub(crate) fn rt<E: std::fmt::Display>(e: E) -> LocalError {
    LocalError::Runtime(e.to_string())
}
