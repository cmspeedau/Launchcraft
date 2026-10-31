//! Filesystem-backed store for uploaded world backup archives.
//!
//! Archives live flat in `<data_dir>/backups/` using the client's own naming
//! (`<world>_<YYYYMMDD>_<HHMMSS>.zip`). Each upload gets a small JSON sidecar
//! (`<name>.meta.json`) holding the [`BackupArchiveInfo`] so listings never
//! re-hash large zips.

use std::fs;
use std::path::{Component, Path, PathBuf};

use launchcraft_core::instance::BackupArchiveInfo;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("invalid archive filename: {0}")]
    InvalidFilename(String),
    #[error("archive not found: {0}")]
    NotFound(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

/// On-disk layout: `<root>/backups/<name>.zip` + `<name>.zip.meta.json`,
/// and `<root>/policy.json` for the server-side upload policy.
#[derive(Clone)]
pub struct BackupStore {
    root: PathBuf,
}

impl BackupStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn backups_dir(&self) -> PathBuf {
        self.root.join("backups")
    }

    fn policy_path(&self) -> PathBuf {
        self.root.join("policy.json")
    }

    pub fn ensure_layout(&self) -> Result<(), StoreError> {
        fs::create_dir_all(self.backups_dir())?;
        Ok(())
    }

    /// Validates a client-supplied filename: a plain `.zip` basename with no
    /// path components. Rejects traversal attempts like `../../evil.zip`.
    pub fn safe_filename(name: &str) -> Result<String, StoreError> {
        let trimmed = name.trim();
        if trimmed.is_empty() || trimmed.len() > 200 {
            return Err(StoreError::InvalidFilename(name.to_string()));
        }
        if !trimmed.to_ascii_lowercase().ends_with(".zip") {
            return Err(StoreError::InvalidFilename(name.to_string()));
        }
        let path = Path::new(trimmed);
        let mut parts = path.components();
        let Some(Component::Normal(basename)) = parts.next() else {
            return Err(StoreError::InvalidFilename(name.to_string()));
        };
        if parts.next().is_some() || basename.to_string_lossy() != trimmed {
            return Err(StoreError::InvalidFilename(name.to_string()));
        }
        // Allowlist keeps names safe for URLs, headers, and the filesystem.
        if trimmed.starts_with('.')
            || !trimmed
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | ' '))
        {
            return Err(StoreError::InvalidFilename(name.to_string()));
        }
        Ok(trimmed.to_string())
    }

    fn archive_path(&self, name: &str) -> PathBuf {
        self.backups_dir().join(name)
    }

    fn meta_path(&self, name: &str) -> PathBuf {
        self.backups_dir().join(format!("{name}.meta.json"))
    }

    /// Stores the archive bytes (overwriting an existing upload of the same
    /// name — uploads are idempotent re-pushes of the same timestamped zip)
    /// and writes the metadata sidecar with the computed BLAKE3 checksum.
    pub fn save(&self, name: &str, bytes: &[u8]) -> Result<BackupArchiveInfo, StoreError> {
        let name = Self::safe_filename(name)?;
        self.ensure_layout()?;

        let checksum = blake3::hash(bytes).to_hex().to_string();
        let info = BackupArchiveInfo {
            file_name: name.clone(),
            world_name: name.split('_').next().unwrap_or(&name).to_string(),
            timestamp: chrono::Utc::now().to_rfc3339(),
            size_bytes: bytes.len() as u64,
            checksum_blake3: checksum,
            is_remote: true,
        };

        write_atomic(&self.archive_path(&name), bytes)?;
        let meta = serde_json::to_vec_pretty(&info)?;
        write_atomic(&self.meta_path(&name), &meta)?;
        Ok(info)
    }

    pub fn load(&self, name: &str) -> Result<Vec<u8>, StoreError> {
        let name = Self::safe_filename(name)?;
        let path = self.archive_path(&name);
        if !path.is_file() {
            return Err(StoreError::NotFound(name));
        }
        Ok(fs::read(path)?)
    }

    /// All stored archives, newest file name first (mirrors `launchcraft-backup`).
    pub fn list(&self) -> Result<Vec<BackupArchiveInfo>, StoreError> {
        let dir = self.backups_dir();
        if !dir.is_dir() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for entry in fs::read_dir(&dir)? {
            let path = entry?.path();
            if path.extension().and_then(|e| e.to_str()) != Some("zip") {
                continue;
            }
            let name = path.file_name().unwrap().to_string_lossy().to_string();
            let meta_path = dir.join(format!("{name}.meta.json"));
            let info = match fs::read(&meta_path)
                .ok()
                .and_then(|b| serde_json::from_slice::<BackupArchiveInfo>(&b).ok())
            {
                Some(info) => info,
                // Sidecar missing or unreadable: recompute from the archive.
                None => {
                    let bytes = fs::read(&path)?;
                    BackupArchiveInfo {
                        file_name: name.clone(),
                        world_name: name.split('_').next().unwrap_or(&name).to_string(),
                        timestamp: chrono::Utc::now().to_rfc3339(),
                        size_bytes: bytes.len() as u64,
                        checksum_blake3: blake3::hash(&bytes).to_hex().to_string(),
                        is_remote: true,
                    }
                }
            };
            out.push(info);
        }
        out.sort_by(|a, b| b.file_name.cmp(&a.file_name));
        Ok(out)
    }

    pub fn delete(&self, name: &str) -> Result<(), StoreError> {
        let name = Self::safe_filename(name)?;
        let path = self.archive_path(&name);
        if !path.is_file() {
            return Err(StoreError::NotFound(name));
        }
        fs::remove_file(&path)?;
        let _ = fs::remove_file(self.meta_path(&name));
        Ok(())
    }

    /// Server-side upload policy. Defaults to enabled when no file exists.
    pub fn policy_enabled(&self) -> bool {
        fs::read(self.policy_path())
            .ok()
            .and_then(|b| {
                serde_json::from_value::<PolicyFile>(serde_json::from_slice(&b).ok()?).ok()
            })
            .map(|p| p.server_backups_enabled)
            .unwrap_or(true)
    }

    pub fn set_policy_enabled(&self, enabled: bool) -> Result<(), StoreError> {
        self.ensure_layout()?;
        let body = serde_json::to_vec_pretty(&PolicyFile {
            server_backups_enabled: enabled,
        })?;
        write_atomic(&self.policy_path(), &body)?;
        Ok(())
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct PolicyFile {
    server_backups_enabled: bool,
}

/// Write to a temp file then rename, so a crash mid-write cannot corrupt data.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), std::io::Error> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_traversal_and_non_zip_names() {
        for bad in ["../evil.zip", "a/b.zip", "C:\\x.zip", "notes.txt", "", ".."] {
            assert!(
                BackupStore::safe_filename(bad).is_err(),
                "must reject {bad:?}"
            );
        }
        assert!(BackupStore::safe_filename("World_20261030_101010.zip").is_ok());
    }

    #[test]
    fn save_list_load_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let store = BackupStore::new(tmp.path());
        let info = store
            .save("Nether_20261030_101010.zip", b"zip-bytes")
            .unwrap();
        assert_eq!(info.world_name, "Nether");
        assert_eq!(
            info.checksum_blake3,
            blake3::hash(b"zip-bytes").to_hex().to_string()
        );

        let list = store.list().unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].checksum_blake3, info.checksum_blake3);
        assert_eq!(
            store.load("Nether_20261030_101010.zip").unwrap(),
            b"zip-bytes"
        );

        store.delete("Nether_20261030_101010.zip").unwrap();
        assert!(store.list().unwrap().is_empty());
        assert!(matches!(
            store.load("Nether_20261030_101010.zip"),
            Err(StoreError::NotFound(_))
        ));
    }

    #[test]
    fn policy_defaults_enabled_and_persists() {
        let tmp = tempfile::tempdir().unwrap();
        let store = BackupStore::new(tmp.path());
        assert!(store.policy_enabled());
        store.set_policy_enabled(false).unwrap();
        assert!(!store.policy_enabled());
    }
}
