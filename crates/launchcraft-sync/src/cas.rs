use crate::error::{SyncError, SyncResult};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use tracing::{debug, trace};

/// Content-Addressable Storage (CAS) for local blob caching.
/// Blobs are addressed by their BLAKE3 hash in a sharded structure: `objects/ab/cd/abcdef...`
pub struct CasStore {
    root_dir: PathBuf,
}

impl CasStore {
    pub fn new<P: AsRef<Path>>(root_dir: P) -> Self {
        Self {
            root_dir: root_dir.as_ref().to_path_buf(),
        }
    }

    /// Returns the storage path for a given BLAKE3 hash.
    pub fn object_path(&self, hash: &str) -> PathBuf {
        let clean_hash = hash.to_lowercase();
        if clean_hash.len() >= 4 {
            self.root_dir
                .join("objects")
                .join(&clean_hash[0..2])
                .join(&clean_hash[2..4])
                .join(&clean_hash)
        } else {
            self.root_dir.join("objects").join(&clean_hash)
        }
    }

    /// Checks if a blob exists in the CAS.
    pub fn exists(&self, hash: &str) -> bool {
        self.object_path(hash).is_file()
    }

    /// Stores a raw stream/buffer into CAS, verifying BLAKE3 integrity.
    /// If `is_zstd_compressed` is true, decompress it before verifying against `expected_hash`.
    pub fn put_blob(
        &self,
        raw_bytes: &[u8],
        expected_hash: &str,
        is_zstd_compressed: bool,
    ) -> SyncResult<PathBuf> {
        let decompressed: Vec<u8> = if is_zstd_compressed {
            zstd::decode_all(raw_bytes)
                .map_err(|e| SyncError::Decompression(format!("Failed to decompress zstd payload: {e}")))?
        } else {
            raw_bytes.to_vec()
        };

        // Compute BLAKE3
        let actual_hash = blake3::hash(&decompressed).to_hex().to_string();
        if !actual_hash.eq_ignore_ascii_case(expected_hash) {
            return Err(SyncError::HashMismatch {
                file: expected_hash.to_string(),
                expected: expected_hash.to_lowercase(),
                actual: actual_hash,
            });
        }

        let target_path = self.object_path(expected_hash);
        if let Some(parent) = target_path.parent() {
            fs::create_dir_all(parent)?;
        }

        // Write atomically via temporary file
        let tmp_path = target_path.with_extension(format!("tmp.{}", std::process::id()));
        {
            let mut file = File::create(&tmp_path)?;
            file.write_all(&decompressed)?;
            file.flush()?;
        }
        fs::rename(tmp_path, &target_path)?;

        debug!(hash = %expected_hash, path = ?target_path, "Stored blob in CAS");
        Ok(target_path)
    }

    /// Materializes a CAS blob into a target file path.
    /// Attempts hard-linking first (saving disk space), falling back to fast copy.
    pub fn materialize_file(&self, hash: &str, destination: &Path) -> SyncResult<()> {
        let src = self.object_path(hash);
        if !src.is_file() {
            return Err(SyncError::Instance(format!(
                "Object with hash {} not found in CAS store",
                hash
            )));
        }

        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }

        // If target already exists, remove it first
        if destination.exists() {
            let _ = fs::remove_file(destination);
        }

        // Try hard-link
        match fs::hard_link(&src, destination) {
            Ok(_) => {
                trace!(source = ?src, destination = ?destination, "Hard-linked blob to destination");
                Ok(())
            }
            Err(_) => {
                // Hard-link fails across filesystem boundaries; fallback to file copy
                fs::copy(&src, destination)?;
                trace!(source = ?src, destination = ?destination, "Copied blob to destination");
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_cas_put_and_materialize() {
        let temp = tempdir().unwrap();
        let cas = CasStore::new(temp.path().join("cas"));

        let content = b"launchcraft modern delta engine";
        let hash = blake3::hash(content).to_hex().to_string();

        let blob_path = cas.put_blob(content, &hash, false).unwrap();
        assert!(blob_path.exists());
        assert!(cas.exists(&hash));

        // Materialize
        let dest = temp.path().join("game").join("mods").join("sample.jar");
        cas.materialize_file(&hash, &dest).unwrap();
        assert!(dest.exists());

        let read_back = fs::read(&dest).unwrap();
        assert_eq!(read_back, content);
    }
}
