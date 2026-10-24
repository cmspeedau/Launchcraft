use reqwest::header::{ETAG, IF_NONE_MATCH};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::info;

#[derive(Error, Debug)]
pub enum DiscoveryError {
    #[error("Network request failed: {0}")]
    Reqwest(#[from] reqwest::Error),
    #[error("Failed to parse manifest: {0}")]
    Serialization(#[from] serde_json::Error),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteFileEntry {
    pub path: String,
    pub hash: String,
    pub size: u64,
    #[serde(default)]
    pub zstd: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemotePackManifest {
    pub id: String,
    pub name: String,
    pub version: String,
    pub game_version: String,
    pub files: Vec<RemoteFileEntry>,
}

#[derive(Debug, Clone)]
pub enum RemoteFetchResult {
    NotModified,
    Updated {
        manifest: RemotePackManifest,
        etag: Option<String>,
    },
}

#[derive(Debug, Clone)]
pub struct RemotePackClient {
    client: Client,
}

impl Default for RemotePackClient {
    fn default() -> Self {
        Self::new()
    }
}

impl RemotePackClient {
    pub fn new() -> Self {
        Self {
            client: Client::builder()
                .user_agent("Launchcraft-Sync/0.1.0")
                .build()
                .unwrap_or_default(),
        }
    }

    /// Fetches remote manifest using HTTP conditional ETag validation.
    pub async fn fetch_manifest(
        &self,
        url: &str,
        cached_etag: Option<&str>,
    ) -> Result<RemoteFetchResult, DiscoveryError> {
        let mut request = self.client.get(url);
        if let Some(etag) = cached_etag {
            request = request.header(IF_NONE_MATCH, etag);
        }

        let response = request.send().await?;

        if response.status() == reqwest::StatusCode::NOT_MODIFIED {
            info!("Remote pack not modified (ETag match)");
            return Ok(RemoteFetchResult::NotModified);
        }

        let response = response.error_for_status()?;
        let etag = response
            .headers()
            .get(ETAG)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());

        let manifest = response.json::<RemotePackManifest>().await?;
        Ok(RemoteFetchResult::Updated { manifest, etag })
    }
}
