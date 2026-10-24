use thiserror::Error;

#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("JSON deserialization error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("Java runtime not found: {0}")]
    JavaNotFound(String),

    #[error("Unsupported platform: {0}")]
    UnsupportedPlatform(String),

    #[error("Version manifest error: {0}")]
    ManifestError(String),

    #[error("Process launch failed: {0}")]
    LaunchFailed(String),
}
