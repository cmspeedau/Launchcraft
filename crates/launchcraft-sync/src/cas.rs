use crate::error::{SyncError, SyncResult};
use std::fs::{self, File};
use std::io::{self, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use thiserror::Error;
use tracing::{debug, info, warn};

#[derive(Error, Debug)]
pub enum CasError {
    #[error("IO error: {0}")]
    Io(#[from] io::Error),
    #[error("Hash mismatch for {hash}: expected {hash}, got {actual}")]
    HashMismatch { hash: String, actual: String },
    #[error("Decompression error: {0}")]
    Decompress(io::Error),
}

/// Content-Addressable Storage (CAS) engine.
/// Stores blobs indexed by their hexadecimal BLAKE3 hash, sharded into two
/// levels of two characters each: `objects/aa/bb/aabb...`
#[derive(Debug, Clone)]
pub struct CasStore {
    base_dir: PathBuf,
}

impl CasStore {
    pub fn new(base_dir: impl Into<PathBuf>) -> Self {
        Self {
            base_dir: base_dir.into(),
        }
    }

    pub fn objects_dir(&self) -> PathBuf {
        self.base_dir.join("objects")
    }

    /// Resolves the filesystem path for a given BLAKE3 hash.
    /// Sharded by the first 4 characters into 2 levels (2 chars each).
    pub fn object_path(&self, hash: &str) -> PathBuf {
        let clean_hash = hash.trim().to_lowercase();
        if clean_hash.len() < 4 {
            return self.objects_dir().join(&clean_hash);
        }
        let shard1 = &clean_hash[0..2];
        let shard2 = &clean_hash[2..4];
        self.objects_dir()
            .join(shard1)
            .join(shard2)
            .join(&clean_hash)
    }

    /// Checks if a blob exists in the CAS.
    pub fn exists(&self, hash: &str) -> bool {
        let path = self.object_path(hash);
        path.is_file()
    }

    /// Verifies the cryptographic BLAKE3 integrity of an existing blob in the CAS.
    pub fn verify_integrity(&self, hash: &str) -> Result<bool, CasError> {
        let path = self.object_path(hash);
        if !path.is_file() {
            return Ok(false);
        }

        let file = File::open(&path)?;
        let mut reader = BufReader::new(file);
        let mut hasher = blake3::Hasher::new();
        io::copy(&mut reader, &mut hasher)?;
        let computed = hasher.finalize().to_hex().to_string();

        Ok(computed.eq_ignore_ascii_case(hash))
    }

    /// Stores a raw stream/reader directly into the CAS after verifying the BLAKE3 digest.
    /// The blob is streamed to a temporary file and persisted atomically.
    pub fn store<R: Read>(&self, expected_hash: &str, mut reader: R) -> Result<PathBuf, CasError> {
        let target_path = self.object_path(expected_hash);
        if self.exists(expected_hash) {
            debug!(
                hash = expected_hash,
                "Blob already exists in CAS; skipping store"
            );
            return Ok(target_path);
        }

        if let Some(parent) = target_path.parent() {
            fs::create_dir_all(parent)?;
        }

        let temp_dir = self.base_dir.join(".tmp");
        fs::create_dir_all(&temp_dir)?;
        let temp_file = tempfile::NamedTempFile::new_in(&temp_dir)?;

        let mut hasher = blake3::Hasher::new();
        let mut buffer = [0u8; 64 * 1024];

        // Scope the writer so its borrow of the temp file ends before persist.
        {
            let mut temp_writer = std::io::BufWriter::new(temp_file.as_file());
            loop {
                let n = reader.read(&mut buffer)?;
                if n == 0 {
                    break;
                }
                hasher.update(&buffer[..n]);
                temp_writer.write_all(&buffer[..n])?;
            }
            temp_writer.flush()?;
        }

        let actual_hash = hasher.finalize().to_hex().to_string();
        if !actual_hash.eq_ignore_ascii_case(expected_hash) {
            return Err(CasError::HashMismatch {
                hash: expected_hash.to_string(),
                actual: actual_hash,
            });
        }

        temp_file.persist(&target_path).map_err(|e| e.error)?;
        debug!(hash = expected_hash, path = ?target_path, "Stored blob into CAS");
        Ok(target_path)
    }

    /// Decompresses a zstd-encoded stream and stores the uncompressed content in the CAS.
    pub fn store_zstd<R: Read>(&self, expected_hash: &str, reader: R) -> Result<PathBuf, CasError> {
        let decoder = zstd::Decoder::new(reader).map_err(CasError::Decompress)?;
        self.store(expected_hash, decoder)
    }

    /// Links or copies a CAS blob into a target file path (e.g., inside instance/minecraft).
    /// Tries hard links first for zero-overhead instant deployment; falls back to copy.
    pub fn deploy_to(&self, hash: &str, destination: &Path) -> Result<(), CasError> {
        let source = self.object_path(hash);
        if !source.is_file() {
            return Err(CasError::Io(io::Error::new(
                io::ErrorKind::NotFound,
                format!("CAS object {} not found", hash),
            )));
        }

        if let Some(parent) = destination.parent() {
            fs::create_dir_all(parent)?;
        }

        if destination.exists() {
            let _ = fs::remove_file(destination);
        }

        // Attempt hard link
        match fs::hard_link(&source, destination) {
            Ok(()) => {
                debug!(source = ?source, dest = ?destination, "Deployed CAS blob via hard-link");
                Ok(())
            }
            Err(err) => {
                warn!(
                    "Hard-link failed ({err}); falling back to file copy: {:?} -> {:?}",
                    source, destination
                );
                fs::copy(&source, destination)?;
                info!(source = ?source, dest = ?destination, "Deployed CAS blob via copy");
                Ok(())
            }
        }
    }

    /// Stores a raw buffer into the CAS, verifying BLAKE3 integrity.
    /// If `is_zstd_compressed` is true, the payload is decompressed before it is
    /// verified against `expected_hash`.
    pub fn put_blob(
        &self,
        raw_bytes: &[u8],
        expected_hash: &str,
        is_zstd_compressed: bool,
    ) -> SyncResult<PathBuf> {
        let result = if is_zstd_compressed {
            self.store_zstd(expected_hash, raw_bytes)
        } else {
            self.store(expected_hash, raw_bytes)
        };

        result.map_err(|err| match err {
            CasError::Io(io_err) => SyncError::Io(io_err),
            CasError::HashMismatch { hash, actual } => SyncError::HashMismatch {
                file: hash,
                expected: expected_hash.to_lowercase(),
                actual,
            },
            CasError::Decompress(io_err) => SyncError::Decompression(io_err.to_string()),
        })
    }

    /// Materializes a CAS blob into a target file path.
    /// Attempts hard-linking first (saving disk space), falling back to file copy.
    pub fn materialize_file(&self, hash: &str, destination: &Path) -> SyncResult<()> {
        self.deploy_to(hash, destination).map_err(|err| match err {
            CasError::Io(io_err) => SyncError::Io(io_err),
            other => SyncError::Instance(other.to_string()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_cas_store_and_verify() {
        let temp_dir = tempfile::tempdir().unwrap();
        let cas = CasStore::new(temp_dir.path());

        let payload = b"Hello Launchcraft CAS!";
        let expected_hash = blake3::hash(payload).to_hex().to_string();

        let path = cas.store(&expected_hash, &payload[..]).unwrap();
        assert!(path.exists());
        assert!(cas.exists(&expected_hash));
        assert!(cas.verify_integrity(&expected_hash).unwrap());

        // Deployment test
        let dest = temp_dir.path().join("instances/pack/config/test.txt");
        cas.deploy_to(&expected_hash, &dest).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), payload);
    }

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
