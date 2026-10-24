use crate::cas::{CasError, CasStore};
use crate::diff::SyncAction;
use futures_util::stream::{self, StreamExt};
use reqwest::Client;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use thiserror::Error;
use tracing::{debug, error, info};

#[derive(Error, Debug)]
pub enum DownloaderError {
    #[error("Network request failed: {0}")]
    Reqwest(#[from] reqwest::Error),
    #[error("CAS error: {0}")]
    Cas(#[from] CasError),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone, Default)]
pub struct DownloadProgress {
    pub bytes_completed: u64,
    pub total_bytes: u64,
    pub files_completed: usize,
    pub total_files: usize,
}

pub struct CasDownloader {
    client: Client,
    cas: CasStore,
    concurrency: usize,
}

impl CasDownloader {
    pub fn new(cas: CasStore, concurrency: usize) -> Self {
        Self {
            client: Client::new(),
            cas,
            concurrency: concurrency.clamp(1, 32),
        }
    }

    /// Downloads a single blob into CAS.
    pub async fn download_blob(
        &self,
        base_url: &str,
        hash: &str,
        is_zstd: bool,
    ) -> Result<(), DownloaderError> {
        if self.cas.exists(hash) && self.cas.verify_integrity(hash).unwrap_or(false) {
            return Ok(());
        }

        let url = format!("{}/objects/{}", base_url.trim_end_matches('/'), hash);
        debug!(url = %url, hash = %hash, "Downloading blob to CAS");

        let response = self.client.get(&url).send().await?.error_for_status()?;
        let bytes = response.bytes().await?;

        let cas = self.cas.clone();
        let hash = hash.to_string();

        tokio::task::spawn_blocking(move || {
            if is_zstd {
                cas.store_zstd(&hash, &bytes[..])
            } else {
                cas.store(&hash, &bytes[..])
            }
        })
        .await
        .map_err(|e| DownloaderError::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))??;

        Ok(())
    }

    /// Concurrently downloads and deploys all actions to the instance target directory.
    pub async fn sync_instance<F>(
        &self,
        base_url: &str,
        actions: &[SyncAction],
        target_dir: &Path,
        progress_cb: F,
    ) -> Result<(), DownloaderError>
    where
        F: Fn(DownloadProgress) + Send + Sync + 'static,
    {
        let downloads: Vec<_> = actions
            .iter()
            .filter_map(|act| match act {
                SyncAction::Download {
                    rel_path,
                    hash,
                    size,
                    zstd_compressed,
                } => Some((rel_path.clone(), hash.clone(), *size, *zstd_compressed)),
                _ => None,
            })
            .collect();

        let total_bytes: u64 = downloads.iter().map(|(_, _, size, _)| *size).sum();
        let total_files = downloads.len();
        let bytes_completed = Arc::new(AtomicU64::new(0));
        let files_completed = Arc::new(AtomicU64::new(0));
        let progress_cb = Arc::new(progress_cb);

        // Process deletions first
        for act in actions {
            if let SyncAction::Delete { rel_path } = act {
                let path = target_dir.join(rel_path);
                if path.exists() {
                    info!(path = ?path, "Deleting extraneous file");
                    let _ = std::fs::remove_file(path);
                }
            }
        }

        // Parallel download and link
        let stream = stream::iter(downloads)
            .map(|(rel_path, hash, size, zstd)| {
                let this = self;
                let target_dir = target_dir.to_path_buf();
                let bytes_completed = Arc::clone(&bytes_completed);
                let files_completed = Arc::clone(&files_completed);
                let progress_cb = Arc::clone(&progress_cb);
                let base_url = base_url.to_string();

                async move {
                    this.download_blob(&base_url, &hash, zstd).await?;
                    let destination = target_dir.join(&rel_path);
                    this.cas.deploy_to(&hash, &destination)?;

                    bytes_completed.fetch_add(size, Ordering::Relaxed);
                    let done_files = files_completed.fetch_add(1, Ordering::Relaxed) as usize + 1;

                    progress_cb(DownloadProgress {
                        bytes_completed: bytes_completed.load(Ordering::Relaxed),
                        total_bytes,
                        files_completed: done_files,
                        total_files,
                    });

                    Ok::<(), DownloaderError>(())
                }
            })
            .buffer_unordered(self.concurrency);

        let results: Vec<Result<(), DownloaderError>> = stream.collect().await;
        for res in results {
            if let Err(e) = res {
                error!("Sync task failed: {e}");
                return Err(e);
            }
        }

        Ok(())
    }
}
