use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Minecraft profile skin information
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SkinInfo {
    pub id: String,
    pub state: String,
    pub url: String,
    pub variant: String,
}

/// Minecraft player profile
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MinecraftProfile {
    pub id: Uuid,
    pub name: String,
    #[serde(default)]
    pub skins: Vec<SkinInfo>,
}

/// Authenticated Launchcraft account entry
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LaunchcraftAccount {
    /// Minecraft profile UUID
    pub uuid: Uuid,
    /// Player username
    pub username: String,
    /// OAuth refresh token used to obtain renewed credentials
    pub refresh_token: String,
    /// Minecraft access token
    pub access_token: String,
    /// UTC timestamp of when access token expires
    pub expires_at: DateTime<Utc>,
    /// Active skin URL if present
    pub skin_url: Option<String>,
}

impl LaunchcraftAccount {
    pub fn is_expired(&self) -> bool {
        Utc::now() >= self.expires_at
    }

    pub fn expires_soon(&self, threshold_seconds: i64) -> bool {
        let threshold = chrono::Duration::seconds(threshold_seconds);
        Utc::now() + threshold >= self.expires_at
    }
}
