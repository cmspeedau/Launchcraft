//! Save game world backup, ZIP compression, and server-side synchronized storage engine.

pub mod archiver;
pub mod client;
pub mod inspector;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum BackupError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Zip compression error: {0}")]
    Zip(#[from] zip::result::ZipError),

    #[error("HTTP client error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("World folder not found: {0}")]
    WorldNotFound(String),

    #[error("Backup archive not found: {0}")]
    ArchiveNotFound(String),

    #[error("Server-side backups are disabled by server/pack policy")]
    ServerBackupsDisabled,

    #[error("Missing remote backup endpoint URL")]
    MissingEndpoint,

    #[error("Remote server returned error: {status} - {message}")]
    RemoteServer {
        status: u16,
        message: String,
    },
}

pub type Result<T> = std::result::Result<T, BackupError>;
