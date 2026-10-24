use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// A single file record within a built pack manifest.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ManifestEntry {
    /// BLAKE3 hash of the uncompressed file content.
    pub hash: String,
    /// Size of the uncompressed file in bytes.
    pub size: u64,
    /// Size of the zstd-compressed CAS object, when stored compressed.
    pub compressed_size: Option<u64>,
    /// Whether the file may be preserved across syncs per user-override patterns.
    #[serde(default)]
    pub user_override: bool,
    /// Whether the file should keep its executable bit when materialized.
    #[serde(default)]
    pub executable: bool,
}

/// Artifact manifest produced by the pack builder (`manifest.json`) and
/// consumed by launchers syncing the pack.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackManifest {
    pub pack_id: String,
    pub pack_version: String,
    pub game_version: String,
    pub loader: Option<String>,
    pub created_at: DateTime<Utc>,
    /// Manifest files keyed by normalized (`/`-separated) relative path.
    pub files: BTreeMap<String, ManifestEntry>,
    /// Patterns of user-editable files preserved during synchronization.
    #[serde(default)]
    pub user_overrides: Vec<String>,
}
