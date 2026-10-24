use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use uuid::Uuid;

/// Backup and sync configuration for an instance.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BackupConfig {
    /// Whether server-side backups and cloud upload are enabled by the server/pack administrator.
    pub server_backups_enabled: bool,
    /// Remote server endpoint for backup operations (e.g., https://backups.example.com/api).
    pub endpoint_url: Option<String>,
    /// Maximum number of local rolling backups kept per world.
    pub max_local_backups: usize,
    /// Whether to automatically create a backup prior to launching the instance.
    pub auto_backup_on_launch: bool,
}

impl Default for BackupConfig {
    fn default() -> Self {
        Self {
            server_backups_enabled: true,
            endpoint_url: None,
            max_local_backups: 5,
            auto_backup_on_launch: false,
        }
    }
}

/// Information about a Minecraft world save directory.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorldSaveInfo {
    pub folder_name: String,
    pub display_name: String,
    pub last_played_timestamp: u64,
    pub size_bytes: u64,
    pub is_locked: bool,
}

/// Metadata for a zipped world backup archive.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BackupArchiveInfo {
    pub file_name: String,
    pub world_name: String,
    pub timestamp: String,
    pub size_bytes: u64,
    pub checksum_blake3: String,
    pub is_remote: bool,
}

/// Source of an instance: synced from a repository, imported, or manually created.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InstanceSource {
    RemoteSync {
        repo_url: String,
        pack_id: String,
        version: String,
        etag: Option<String>,
        locked: bool,
    },
    Manual {
        created_at: chrono::DateTime<chrono::Utc>,
    },
    Imported {
        format: String, // e.g., "mrpack", "curseforge"
        source_file: Option<String>,
    },
}

/// Metadata representation of a local instance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstanceConfig {
    pub id: Uuid,
    pub name: String,
    pub minecraft_version: String,
    pub mod_loader: Option<ModLoaderConfig>,
    pub java_path: Option<PathBuf>,
    pub jvm_args: Vec<String>,
    pub source: InstanceSource,
    pub icon_path: Option<PathBuf>,
    #[serde(default)]
    pub backup_config: BackupConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModLoaderConfig {
    pub loader_type: String, // "fabric", "neoforge", "forge", "quilt"
    pub loader_version: String,
}

/// A single file entry within a synced pack manifest.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileEntry {
    /// Normalized (`/`-separated) path of the file relative to the game directory.
    pub path: String,
    /// BLAKE3 hash of the uncompressed content.
    pub hash: String,
    /// Uncompressed size in bytes.
    pub size: u64,
    /// Encoding of the stored CAS object, e.g. `"zstd"`, when compressed.
    #[serde(default)]
    pub compressed: Option<String>,
}

/// Pack manifest fetched from a remote repository for instance synchronization.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub files: Vec<FileEntry>,
}

/// Configuration of the remote repository an instance syncs from.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RepositoryConfig {
    pub url: String,
    #[serde(default)]
    pub etag: Option<String>,
}
