use crate::instance::{FileEntry, Manifest};
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

impl From<&PackManifest> for Manifest {
    /// Converts a built pack manifest into the flat file-entry form consumed
    /// by the sync engine. Blobs with a `compressed_size` are served zstd
    /// encoded by the builder, which is the only remote encoding today.
    fn from(pack: &PackManifest) -> Self {
        let files = pack
            .files
            .iter()
            .map(|(path, entry)| FileEntry {
                path: path.clone(),
                hash: entry.hash.clone(),
                size: entry.size,
                compressed: entry.compressed_size.map(|_| "zstd".to_string()),
            })
            .collect();
        Manifest { files }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_manifest_converts_to_sync_manifest() {
        let mut files = BTreeMap::new();
        files.insert(
            "mods/example.jar".to_string(),
            ManifestEntry {
                hash: "AB12".repeat(16),
                size: 1234,
                compressed_size: Some(900),
                user_override: false,
                executable: false,
            },
        );
        files.insert(
            "config/override.cfg".to_string(),
            ManifestEntry {
                hash: "CD34".repeat(16),
                size: 10,
                compressed_size: None,
                user_override: true,
                executable: false,
            },
        );

        let pack = PackManifest {
            pack_id: "test-pack".into(),
            pack_version: "1.0.0".into(),
            game_version: "1.20.4".into(),
            loader: None,
            created_at: Utc::now(),
            files,
            user_overrides: vec!["config/override.cfg".into()],
        };

        let manifest = Manifest::from(&pack);
        assert_eq!(manifest.files.len(), 2);

        let jar = manifest
            .files
            .iter()
            .find(|f| f.path == "mods/example.jar")
            .unwrap();
        assert_eq!(jar.size, 1234);
        assert_eq!(jar.compressed.as_deref(), Some("zstd"));
        assert_eq!(jar.hash, "AB12".repeat(16));

        let cfg = manifest
            .files
            .iter()
            .find(|f| f.path == "config/override.cfg")
            .unwrap();
        assert_eq!(cfg.compressed, None);
    }
}
