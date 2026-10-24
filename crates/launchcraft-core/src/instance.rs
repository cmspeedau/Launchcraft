use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use uuid::Uuid;

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
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModLoaderConfig {
    pub loader_type: String, // "fabric", "neoforge", "forge", "quilt"
    pub loader_version: String,
}
