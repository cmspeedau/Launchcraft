//! Synchronizes world backup ZIPs with remote servers, respecting server-side enablement policies.

use std::fs::File;
use std::io::Write;
use std::path::Path;
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use launchcraft_core::instance::{BackupArchiveInfo, BackupConfig};
use crate::inspector::WorldInspector;
use crate::{BackupError, Result};

/// Client for remote world backup synchronization.
pub struct BackupSyncClient {
    client: reqwest::Client,
}

impl Default for BackupSyncClient {
    fn default() -> Self {
        Self::new()
    }
}

impl BackupSyncClient {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::builder().build().unwrap(),
        }
    }

    /// Uploads a zipped world backup to the remote server.
    ///
    /// CRITICAL: Validates that `config.server_backups_enabled` is true.
    /// If disabled server-side, this returns `BackupError::ServerBackupsDisabled`.
    pub async fn upload_backup(
        &self,
        instance_dir: &Path,
        config: &BackupConfig,
        archive_name: &str,
        auth_token: Option<&str>,
    ) -> Result<BackupArchiveInfo> {
        // Enforce server-side toggle policy
        if !config.server_backups_enabled {
            return Err(BackupError::ServerBackupsDisabled);
        }

        let endpoint = config
            .endpoint_url
            .as_deref()
            .ok_or(BackupError::MissingEndpoint)?;

        let backups_dir = WorldInspector::get_backups_dir(instance_dir);
        let archive_path = backups_dir.join(archive_name);

        if !archive_path.is_file() {
            return Err(BackupError::ArchiveNotFound(archive_name.to_string()));
        }

        let file_bytes = tokio::fs::read(&archive_path).await?;
        let checksum = blake3::hash(&file_bytes).to_hex().to_string();

        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/zip"));
        if let Some(token) = auth_token {
            if let Ok(val) = HeaderValue::from_str(&format!("Bearer {}", token)) {
                headers.insert(AUTHORIZATION, val);
            }
        }

        let upload_url = format!("{}/backups/upload?filename={}", endpoint.trim_end_matches('/'), archive_name);
        let response = self
            .client
            .post(&upload_url)
            .headers(headers)
            .body(file_bytes)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let text = response.text().await.unwrap_or_default();
            return Err(BackupError::RemoteServer {
                status,
                message: text,
            });
        }

        let world_name = archive_name.split('_').next().unwrap_or(archive_name).to_string();
        Ok(BackupArchiveInfo {
            file_name: archive_name.to_string(),
            world_name,
            timestamp: chrono::Utc::now().to_rfc3339(),
            size_bytes: archive_path.metadata()?.len(),
            checksum_blake3: checksum,
            is_remote: true,
        })
    }

    /// Downloads a world backup ZIP from the remote server into the instance's `backups/` directory.
    pub async fn download_backup(
        &self,
        instance_dir: &Path,
        config: &BackupConfig,
        archive_name: &str,
        auth_token: Option<&str>,
    ) -> Result<BackupArchiveInfo> {
        let endpoint = config
            .endpoint_url
            .as_deref()
            .ok_or(BackupError::MissingEndpoint)?;

        let mut request = self.client.get(format!(
            "{}/backups/download?filename={}",
            endpoint.trim_end_matches('/'),
            archive_name
        ));

        if let Some(token) = auth_token {
            request = request.bearer_auth(token);
        }

        let response = request.send().await?;
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let text = response.text().await.unwrap_or_default();
            return Err(BackupError::RemoteServer {
                status,
                message: text,
            });
        }

        let bytes = response.bytes().await?;
        let checksum = blake3::hash(&bytes).to_hex().to_string();

        let backups_dir = WorldInspector::get_backups_dir(instance_dir);
        tokio::fs::create_dir_all(&backups_dir).await?;

        let destination = backups_dir.join(archive_name);
        let mut file = File::create(&destination)?;
        file.write_all(&bytes)?;

        let world_name = archive_name.split('_').next().unwrap_or(archive_name).to_string();
        Ok(BackupArchiveInfo {
            file_name: archive_name.to_string(),
            world_name,
            timestamp: chrono::Utc::now().to_rfc3339(),
            size_bytes: bytes.len() as u64,
            checksum_blake3: checksum,
            is_remote: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn test_upload_fails_when_server_disabled() {
        let tmp = tempdir().unwrap();
        let client = BackupSyncClient::new();
        let config = BackupConfig {
            server_backups_enabled: false,
            endpoint_url: Some("https://example.com/api".to_string()),
            max_local_backups: 5,
            auto_backup_on_launch: false,
        };

        let result = client
            .upload_backup(tmp.path(), &config, "mock.zip", None)
            .await;

        match result {
            Err(BackupError::ServerBackupsDisabled) => {}
            _ => panic!("Expected ServerBackupsDisabled error"),
        }
    }
}
