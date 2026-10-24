use crate::whitelist::WhitelistMatcher;
use launchcraft_core::instance::{FileEntry, Manifest};
use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncAction {
    /// File is already in place with matching hash
    Keep { path: String },
    /// File needs to be downloaded or materialized from CAS
    Download(FileEntry),
    /// Extraneous file on disk that is not in manifest and not whitelisted
    Delete { relative_path: String, full_path: PathBuf },
}

#[derive(Debug, Default)]
pub struct SyncPlan {
    pub keep_count: usize,
    pub downloads: Vec<FileEntry>,
    pub deletions: Vec<PathBuf>,
}

pub struct InstanceDiffer {
    whitelist: WhitelistMatcher,
}

impl InstanceDiffer {
    pub fn new(whitelist: WhitelistMatcher) -> Self {
        Self { whitelist }
    }

    /// Hash a local file using BLAKE3
    pub fn compute_file_hash(path: &Path) -> std::io::Result<String> {
        let mut file = File::open(path)?;
        let mut hasher = blake3::Hasher::new();
        let mut buffer = [0u8; 65536];

        loop {
            let n = file.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            hasher.update(&buffer[..n]);
        }

        Ok(hasher.finalize().to_hex().to_string())
    }

    /// Compare local directory state against target manifest
    pub fn diff_instance(
        &self,
        game_dir: &Path,
        target_manifest: &Manifest,
    ) -> std::io::Result<SyncPlan> {
        let mut plan = SyncPlan::default();

        // Index manifest entries by normalized relative path
        let mut manifest_map: HashMap<String, &FileEntry> = HashMap::new();
        for entry in &target_manifest.files {
            let norm = entry.path.replace('\\', "/");
            manifest_map.insert(norm, entry);
        }

        // Scan current local files
        let mut local_found: HashMap<String, PathBuf> = HashMap::new();
        if game_dir.is_dir() {
            for entry in WalkDir::new(game_dir).follow_links(false).into_iter().filter_map(|e| e.ok()) {
                if !entry.file_type().is_file() {
                    continue;
                }

                let full_path = entry.path().to_path_buf();
                if let Ok(rel) = full_path.strip_prefix(game_dir) {
                    let rel_norm = rel.to_string_lossy().replace('\\', "/");
                    local_found.insert(rel_norm, full_path);
                }
            }
        }

        // Determine which manifest entries are satisfied or need download
        for (rel_path, entry) in &manifest_map {
            if let Some(local_path) = local_found.remove(rel_path) {
                // File exists locally, check hash
                let matches = match Self::compute_file_hash(&local_path) {
                    Ok(h) => h.eq_ignore_ascii_case(&entry.hash),
                    Err(_) => false,
                };

                if matches {
                    plan.keep_count += 1;
                } else {
                    plan.downloads.push((*entry).clone());
                }
            } else {
                // File missing locally
                plan.downloads.push((*entry).clone());
            }
        }

        // Local files not in target manifest: check if whitelisted or delete
        for (rel_path, full_path) in local_found {
            if !self.whitelist.is_whitelisted(&rel_path) {
                plan.deletions.push(full_path);
            }
        }

        Ok(plan)
    }
}
