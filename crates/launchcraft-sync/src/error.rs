use std::io;
use thiserror::Error;

pub type SyncResult<T> = Result<T, SyncError>;

#[derive(Error, Debug)]
pub enum SyncError {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),

    #[error("HTTP request error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("JSON serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("Decompression error: {0}")]
    Decompression(String),

    #[error("Hash mismatch for {file}: expected {expected}, calculated {actual}")]
    HashMismatch {
        file: String,
        expected: String,
        actual: String,
    },

    #[error("Remote repository error ({status}): {message}")]
    RemoteRepo { status: u16, message: String },

    #[error("Instance error: {0}")]
    Instance(String),
}
