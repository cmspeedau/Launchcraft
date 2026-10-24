use std::path::PathBuf;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AuthError {
    #[error("HTTP request error: {0}")]
    Network(#[from] reqwest::Error),

    #[error("Serialization / Deserialization error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("OAuth error: {error} - {description}")]
    OAuth {
        error: String,
        description: String,
    },

    #[error("Xbox Live authentication failed with code: {0}")]
    XboxLiveError(u64),

    #[error("XSTS authentication error: {0}")]
    XstsError(String),

    #[error("Minecraft service error: {0}")]
    MinecraftService(String),

    #[error("Account does not own Minecraft: Java Edition")]
    GameNotOwned,

    #[error("Account not found: {0}")]
    AccountNotFound(String),

    #[error("Storage error: {0}")]
    Storage(String),
}

pub type Result<T> = std::result::Result<T, AuthError>;
