use crate::cas::CasStore;
use crate::diff::InstanceDiffer;
use crate::error::{SyncError, SyncResult};
use crate::whitelist::WhitelistMatcher;
use launchcraft_core::instance::{FileEntry, Manifest, RepositoryConfig};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use tracing::{debug, info, warn};

#[derive(Debug, Clone)]
pub struct SyncProgress {
    pub files_total: usize,
    pub files_completed: usize,
    pub bytes_total: u64,
    pub bytes_downloaded: u64,
    pub current_operation: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncStatus {
    Idle,
    DiscoveringRepository,
    CheckingIntegrity,
    Downloading,
    ApplyingChanges,
    Completed,
}

pub struct SyncEngine {
    client: reqwest::Client,
    cas: Arc<CasStore>,
    whitelist: WhitelistMatcher,
}

impl SyncEngine {
    pub fn new(cas: Arc<CasStore>) -> Self {
        let client = reqwest::Client::builder()
            .user_agent("Launchcraft/0.1.0")
            .build()
            .unwrap_or_default();

        Self {
            client,
            cas,
            whitelist: WhitelistMatcher::default(),
        }
    }

    pub fn with_whitelist(mut self, whitelist: WhitelistMatcher) -> Self {
        self.whitelist = whitelist;
        self
    }

    /// Fetch remote repository index and pack manifest
    pub async fn fetch_remote_manifest(&self, repo: &RepositoryConfig) -> SyncResult<Manifest> {
        let url = format!("{}/manifest.json", repo.url.trim_end_matches('/'));
        info!(url = %url, "Querying remote modpack manifest");

        let mut request = self.client.get(&url);
        if let Some(etag) = &repo.etag {
            request = request.header("If-None-Match", etag);
        }

        let response = request.send().await?;
        if !response.status().is_success() {
            return Err(SyncError::RemoteRepo {
                status: response.status().as_u16(),
                message: format!("Failed to fetch remote manifest from {}", url),
            });
        }

        let manifest: Manifest = response.json().await?;
        Ok(manifest)
    }

    /// Execute synchronization for an instance directory given a target manifest
    pub async fn synchronize(
        &self,
        base_url: &str,
        game_dir: &Path,
        manifest: &Manifest,
        progress_cb: Option<Arc<dyn Fn(SyncProgress) + Send + Sync>>,
    ) -> SyncResult<()> {
        let differ = InstanceDiffer::new(self.whitelist.clone());
        let plan = differ.diff_instance(game_dir, manifest)?;

        info!(
            keep = plan.keep_count,
            downloads = plan.downloads.len(),
            deletions = plan.deletions.len(),
            "Calculated sync plan"
        );

        let total_bytes: u64 = plan.downloads.iter().map(|f| f.size).sum();
        let downloaded_bytes = Arc::new(AtomicU64::new(0));
        let completed_files = Arc::new(AtomicUsize::new(0));
        let total_files = plan.downloads.len();

        // 1. Download missing blobs to CAS
        for entry in plan.downloads {
            // Check if already in CAS store
            if !self.cas.exists(&entry.hash) {
                if let Some(ref cb) = progress_cb {
                    cb(SyncProgress {
                        files_total: total_files,
                        files_completed: completed_files.load(Ordering::Relaxed),
                        bytes_total: total_bytes,
                        bytes_downloaded: downloaded_bytes.load(Ordering::Relaxed),
                        current_operation: format!("Downloading {}", entry.path),
                    });
                }

                self.download_entry_to_cas(base_url, &entry).await?;
            }

            // Materialize into game dir
            let target_path = game_dir.join(&entry.path);
            self.cas.materialize_file(&entry.hash, &target_path)?;

            downloaded_bytes.fetch_add(entry.size, Ordering::Relaxed);
            completed_files.fetch_add(1, Ordering::Relaxed);
        }

        // 2. Perform deletions of obsolete files
        for obsolete in plan.deletions {
            debug!(path = ?obsolete, "Removing extraneous file");
            let _ = fs::remove_file(obsolete);
        }

        if let Some(ref cb) = progress_cb {
            cb(SyncProgress {
                files_total: total_files,
                files_completed: total_files,
                bytes_total: total_bytes,
                bytes_downloaded: total_bytes,
                current_operation: "Synchronization complete".into(),
            });
        }

        Ok(())
    }

    async fn download_entry_to_cas(&self, base_url: &str, entry: &FileEntry) -> SyncResult<PathBuf> {
        let object_url = format!("{}/objects/{}", base_url.trim_end_matches('/'), entry.hash);
        debug!(url = %object_url, "Downloading blob into CAS");

        let response = self.client.get(&object_url).send().await?;
        if !response.status().is_success() {
            return Err(SyncError::RemoteRepo {
                status: response.status().as_u16(),
                message: format!("Failed to download object {}", entry.hash),
            });
        }

        let bytes = response.bytes().await?;
        let is_zstd = entry.compressed.as_deref() == Some("zstd");

        self.cas.put_blob(&bytes, &entry.hash, is_zstd)
    }
}
