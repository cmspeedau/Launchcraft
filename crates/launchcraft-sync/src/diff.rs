use crate::whitelist::WhitelistMatcher;
use launchcraft_core::instance::{FileEntry, Manifest};
use rayon::prelude::*;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{self, BufReader, Read};
use std::path::{Path, PathBuf};
use tracing::debug;
use walkdir::WalkDir;

/// File action determined during manifest diffing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncAction {
    Download {
        rel_path: String,
        hash: String,
        size: u64,
        zstd_compressed: bool,
    },
    Delete {
        rel_path: String,
    },
    Unchanged {
        rel_path: String,
    },
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SyncDiff {
    pub to_download: Vec<SyncAction>,
    pub to_delete: Vec<SyncAction>,
    pub unchanged: Vec<SyncAction>,
}

impl SyncDiff {
    pub fn is_up_to_date(&self) -> bool {
        self.to_download.is_empty() && self.to_delete.is_empty()
    }
}

/// Files and folders that should NEVER be deleted during synchronization.
const PRESERVED_PREFIXES: &[&str] = &[
    "saves/",
    "screenshots/",
    "resourcepacks/",
    "shaderpacks/",
    "crash-reports/",
    "logs/",
];

const PRESERVED_EXACT: &[&str] = &[
    "options.txt",
    "optionsof.txt",
    "servers.dat",
    "usercache.json",
    "usernamecache.json",
];

pub fn is_protected_user_path(rel_path: &str) -> bool {
    let normalized = rel_path.replace('\\', "/");
    let lower = normalized.to_lowercase();

    for exact in PRESERVED_EXACT {
        if lower == *exact {
            return true;
        }
    }

    for prefix in PRESERVED_PREFIXES {
        if lower.starts_with(prefix) {
            return true;
        }
    }

    false
}

/// Computes BLAKE3 hash for a local file.
pub fn hash_local_file(path: &Path) -> io::Result<String> {
    let file = File::open(path)?;
    let mut reader = BufReader::new(file);
    let mut hasher = blake3::Hasher::new();
    io::copy(&mut reader, &mut hasher)?;
    Ok(hasher.finalize().to_hex().to_string())
}

/// Scans local directory in parallel and returns a map of relative_path -> blake3 hash.
pub fn scan_local_directory(root: &Path) -> HashMap<String, String> {
    if !root.exists() {
        return HashMap::new();
    }

    let entries: Vec<PathBuf> = WalkDir::new(root)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .map(|e| e.into_path())
        .collect();

    entries
        .into_par_iter()
        .filter_map(|file_path| {
            let rel = pathdiff_relative(&file_path, root)?;
            if is_protected_user_path(&rel) {
                return None;
            }
            let hash = hash_local_file(&file_path).ok()?;
            Some((rel, hash))
        })
        .collect()
}

fn pathdiff_relative(file_path: &Path, root: &Path) -> Option<String> {
    let rel = file_path.strip_prefix(root).ok()?;
    Some(rel.to_string_lossy().replace('\\', "/"))
}

/// Computes the diff between expected manifest entries and local files.
pub fn compute_diff(
    local_files: &HashMap<String, String>,
    expected_entries: &[(String, String, u64, bool)], // (rel_path, hash, size, zstd)
) -> SyncDiff {
    let mut diff = SyncDiff::default();
    let mut expected_map = HashSet::new();

    for (rel_path, hash, size, zstd) in expected_entries {
        expected_map.insert(rel_path.clone());
        if let Some(local_hash) = local_files.get(rel_path) {
            if local_hash.eq_ignore_ascii_case(hash) {
                diff.unchanged.push(SyncAction::Unchanged {
                    rel_path: rel_path.clone(),
                });
                continue;
            }
        }
        diff.to_download.push(SyncAction::Download {
            rel_path: rel_path.clone(),
            hash: hash.clone(),
            size: *size,
            zstd_compressed: *zstd,
        });
    }

    for local_path in local_files.keys() {
        if !expected_map.contains(local_path) && !is_protected_user_path(local_path) {
            diff.to_delete.push(SyncAction::Delete {
                rel_path: local_path.clone(),
            });
        }
    }

    debug!(
        downloads = diff.to_download.len(),
        deletions = diff.to_delete.len(),
        unchanged = diff.unchanged.len(),
        "Computed manifest diff"
    );

    diff
}

#[derive(Debug, Default)]
pub struct SyncPlan {
    pub keep_count: usize,
    pub downloads: Vec<FileEntry>,
    pub deletions: Vec<PathBuf>,
}

/// Compares local instance state against a target manifest, honoring the
/// user-file whitelist when scheduling deletions.
pub struct InstanceDiffer {
    whitelist: WhitelistMatcher,
}

impl InstanceDiffer {
    pub fn new(whitelist: WhitelistMatcher) -> Self {
        Self { whitelist }
    }

    /// Hash a local file using BLAKE3
    pub fn compute_file_hash(path: &Path) -> io::Result<String> {
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
    ) -> io::Result<SyncPlan> {
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
            for entry in WalkDir::new(game_dir)
                .follow_links(false)
                .into_iter()
                .filter_map(|e| e.ok())
            {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_preserved_paths() {
        assert!(is_protected_user_path("options.txt"));
        assert!(is_protected_user_path("saves/World1/level.dat"));
        assert!(is_protected_user_path("screenshots/pic.png"));
        assert!(!is_protected_user_path("mods/jei.jar"));
    }

    #[test]
    fn test_compute_diff() {
        let mut local = HashMap::new();
        local.insert("mods/foo.jar".to_string(), "hash_a".to_string());
        local.insert("mods/old.jar".to_string(), "hash_old".to_string());

        let expected = vec![
            ("mods/foo.jar".to_string(), "hash_a".to_string(), 100, false),
            ("mods/bar.jar".to_string(), "hash_b".to_string(), 200, true),
        ];

        let diff = compute_diff(&local, &expected);
        assert_eq!(diff.unchanged.len(), 1);
        assert_eq!(diff.to_download.len(), 1);
        assert_eq!(diff.to_delete.len(), 1);
    }
}
