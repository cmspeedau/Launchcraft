use serde::{Deserialize, Serialize};

/// Remote repository definition (e.g. subscribed community or server repository)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepositoryEntry {
    pub id: String,
    pub name: String,
    pub url: String,
    pub enabled: bool,
}

/// Manifest of packs exposed by a remote repository (packages.json)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepositoryManifest {
    pub name: String,
    pub description: Option<String>,
    pub packs: Vec<RemotePackSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemotePackSummary {
    pub id: String,
    pub name: String,
    pub summary: Option<String>,
    pub version: String,
    pub minecraft_version: String,
    pub loader: Option<String>,
    pub manifest_url: String,
    pub icon_url: Option<String>,
}
