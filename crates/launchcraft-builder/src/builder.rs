use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use rayon::prelude::*;
use walkdir::WalkDir;

use launchcraft_core::manifest::{ManifestEntry, PackManifest};
use crate::config::BuildConfig;
use crate::error::BuilderError;

pub struct PackBuilder {
    input_dir: PathBuf,
    output_dir: PathBuf,
    config: BuildConfig,
}

#[derive(Debug)]
struct ScannedFile {
    relative_path: String,
    absolute_path: PathBuf,
    size: u64,
}

impl PackBuilder {
    pub fn new(input_dir: impl Into<PathBuf>, output_dir: impl Into<PathBuf>, config: BuildConfig) -> Self {
        Self {
            input_dir: input_dir.into(),
            output_dir: output_dir.into(),
            config,
        }
    }

    pub fn build(&self) -> Result<PackManifest, BuilderError> {
        tracing::info!(
            input = ?self.input_dir,
            output = ?self.output_dir,
            pack_id = %self.config.pack_id,
            "Starting modpack build process"
        );

        if !self.input_dir.exists() {
            return Err(BuilderError::Build(format!(
                "Input directory does not exist: {}",
                self.input_dir.display()
            )));
        }

        let cas_objects_dir = self.output_dir.join("cas").join("objects");
        fs::create_dir_all(&cas_objects_dir)?;

        let scanned_files = self.scan_input_files()?;
        tracing::info!("Found {} files to process", scanned_files.len());

        let entries: Vec<(String, ManifestEntry)> = scanned_files
            .into_par_iter()
            .map(|scanned| self.process_file(&scanned, &cas_objects_dir))
            .collect::<Result<Vec<_>, BuilderError>>()?;

        let mut manifest_entries = BTreeMap::new();
        for (rel_path, entry) in entries {
            manifest_entries.insert(rel_path, entry);
        }

        let manifest = PackManifest {
            pack_id: self.config.pack_id.clone(),
            pack_version: self.config.version.clone(),
            game_version: self.config.game_version.clone(),
            loader: None,
            created_at: chrono::Utc::now(),
            files: manifest_entries,
            user_overrides: self.config.user_overrides.clone(),
        };

        let manifest_path = self.output_dir.join("manifest.json");
        let manifest_json = serde_json::to_string_pretty(&manifest)?;
        fs::write(&manifest_path, manifest_json)?;

        tracing::info!(
            total_files = manifest.files.len(),
            manifest = ?manifest_path,
            "Modpack build complete successfully"
        );

        Ok(manifest)
    }

    fn scan_input_files(&self) -> Result<Vec<ScannedFile>, BuilderError> {
        let mut results = Vec::new();

        for entry in WalkDir::new(&self.input_dir)
            .follow_links(false)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            let path = entry.path();
            if path.is_file() {
                let rel = path
                    .strip_prefix(&self.input_dir)
                    .map_err(|e| BuilderError::Build(e.to_string()))?;

                let rel_str = rel.to_string_lossy().replace('\\', "/");
                // pack.json is the builder's own configuration, not a game file.
                if rel_str == "pack.json" {
                    continue;
                }
                let metadata = entry.metadata()?;

                results.push(ScannedFile {
                    relative_path: rel_str,
                    absolute_path: path.to_path_buf(),
                    size: metadata.len(),
                });
            }
        }

        Ok(results)
    }

    fn process_file(
        &self,
        scanned: &ScannedFile,
        cas_objects_dir: &Path,
    ) -> Result<(String, ManifestEntry), BuilderError> {
        let mut file = BufReader::new(File::open(&scanned.absolute_path)?);
        let mut buffer = Vec::with_capacity(scanned.size as usize);
        file.read_to_end(&mut buffer)?;

        let hash_raw = blake3::hash(&buffer);
        let hash = hash_raw.to_hex().to_string();

        let compressed_payload = zstd::encode_all(&buffer[..], self.config.compression_level)
            .map_err(|e| BuilderError::Compression(e.to_string()))?;

        let compressed_size = compressed_payload.len() as u64;

        let shard_prefix = &hash[0..2];
        let shard_dir = cas_objects_dir.join(shard_prefix);
        fs::create_dir_all(&shard_dir)?;

        let blob_path = shard_dir.join(&hash);
        if !blob_path.exists() {
            let mut blob_file = File::create(&blob_path)?;
            blob_file.write_all(&compressed_payload)?;
        }

        let is_user_override = self.config.user_overrides.iter().any(|pattern| {
            if pattern.ends_with('/') {
                scanned.relative_path.starts_with(pattern)
            } else {
                scanned.relative_path == *pattern
            }
        });

        let entry = ManifestEntry {
            hash,
            size: scanned.size,
            compressed_size: Some(compressed_size),
            user_override: is_user_override,
            executable: false,
        };

        Ok((scanned.relative_path.clone(), entry))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_pack_builder_generates_manifest_and_cas() {
        let temp_in = TempDir::new().unwrap();
        let temp_out = TempDir::new().unwrap();

        let mods_dir = temp_in.path().join("mods");
        fs::create_dir_all(&mods_dir).unwrap();
        fs::write(mods_dir.join("example-mod.jar"), b"dummy mod content").unwrap();

        let config_dir = temp_in.path().join("config");
        fs::create_dir_all(&config_dir).unwrap();
        fs::write(config_dir.join("test.cfg"), b"key=value").unwrap();

        let cfg = BuildConfig {
            pack_id: "test-pack".to_string(),
            title: "Test Pack".to_string(),
            version: "1.0.0".to_string(),
            game_version: "1.20.4".to_string(),
            summary: None,
            user_overrides: vec!["config/test.cfg".to_string()],
            compression_level: 3,
        };

        let builder = PackBuilder::new(temp_in.path(), temp_out.path(), cfg);
        let manifest = builder.build().unwrap();

        assert_eq!(manifest.pack_id, "test-pack");
        assert_eq!(manifest.files.len(), 2);
        assert!(manifest.files.contains_key("mods/example-mod.jar"));
        assert!(manifest.files.contains_key("config/test.cfg"));
        assert!(manifest.files.get("config/test.cfg").unwrap().user_override);

        let manifest_file = temp_out.path().join("manifest.json");
        assert!(manifest_file.exists());

        let cas_dir = temp_out.path().join("cas").join("objects");
        assert!(cas_dir.exists());
    }
}
