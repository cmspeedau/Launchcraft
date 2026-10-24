use thiserror::Error;

#[derive(Debug, Error)]
pub enum BuilderError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON serialization error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("Compression error: {0}")]
    Compression(String),

    #[error("Build failure: {0}")]
    Build(String),
}
