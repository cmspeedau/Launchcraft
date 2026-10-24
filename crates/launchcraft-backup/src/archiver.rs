//! Creates and extracts zipped world backup archives.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use walkdir::WalkDir;
use zip::write::SimpleFileOptions;
use zip::{ZipArchive, ZipWriter};
use launchcraft_core::instance::BackupArchiveInfo;
use crate::inspector::WorldInspector;
use crate::{BackupError, Result};

/// World archiver for creating and restoring `.zip` archives of Minecraft save games.
pub struct WorldArchiver;

impl WorldArchiver {
    /// Compresses a target world folder into a ZIP archive saved in `instances/<instance>/backups/`.
    pub fn create_backup(instance_dir: &Path, world_folder_name: &str) -> Result<BackupArchiveInfo> {
        let saves_dir = WorldInspector::get_saves_dir(instance_dir);
        let world_path = saves_dir.join(world_folder_name);

        if !world_path.is_dir() {
            return Err(BackupError::WorldNotFound(world_folder_name.to_string()));
        }

        let backups_dir = WorldInspector::get_backups_dir(instance_dir);
        fs::create_dir_all(&backups_dir)?;

        let timestamp_str = chrono::Utc::now().format("%Y%m%d_%H%M%S").to_string();
        let archive_filename = format!("{}_{}.zip", world_folder_name, timestamp_str);
        let archive_path = backups_dir.join(&archive_filename);

        let file = File::create(&archive_path)?;
        let mut zip = ZipWriter::new(file);
        let options = SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);

        let mut hasher = blake3::Hasher::new();

        for entry in WalkDir::new(&world_path).into_iter().flatten() {
            let path = entry.path();
            let relative_path = path.strip_prefix(&world_path).map_err(|e| {
                std::io::Error::new(std::io::ErrorKind::Other, e.to_string())
            })?;

            if relative_path.as_os_str().is_empty() {
                continue;
            }

            // Exclude temporary session locks
            if relative_path.to_string_lossy() == "session.lock" {
                continue;
            }

            let name = relative_path.to_string_lossy().replace('\\', "/");

            if entry.file_type().is_dir() {
                zip.add_directory(format!("{}/", name), options)?;
            } else if entry.file_type().is_file() {
                zip.start_file(name, options)?;
                let mut f = File::open(path)?;
                let mut buffer = Vec::new();
                f.read_to_end(&mut buffer)?;
                zip.write_all(&buffer)?;
                hasher.update(&buffer);
            }
        }

        zip.finish()?;

        let metadata = fs::metadata(&archive_path)?;
        let checksum = hasher.finalize().to_hex().to_string();

        Ok(BackupArchiveInfo {
            file_name: archive_filename,
            world_name: world_folder_name.to_string(),
            timestamp: chrono::Utc::now().to_rfc3339(),
            size_bytes: metadata.len(),
            checksum_blake3: checksum,
            is_remote: false,
        })
    }

    /// Restores a zipped backup archive into the instance's Minecraft `saves/` folder.
    pub fn restore_backup(
        instance_dir: &Path,
        archive_name: &str,
        target_world_name: Option<&str>,
    ) -> Result<PathBuf> {
        let backups_dir = WorldInspector::get_backups_dir(instance_dir);
        let archive_path = backups_dir.join(archive_name);

        if !archive_path.is_file() {
            return Err(BackupError::ArchiveNotFound(archive_name.to_string()));
        }

        let saves_dir = WorldInspector::get_saves_dir(instance_dir);
        fs::create_dir_all(&saves_dir)?;

        let dest_name = match target_world_name {
            Some(name) => name.to_string(),
            None => {
                let stem = archive_path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("world_restored");
                // Remove timestamp suffix if format is name_YYYYMMDD_HHMMSS
                stem.split('_')
                    .next()
                    .unwrap_or(stem)
                    .to_string()
            }
        };

        let dest_path = saves_dir.join(&dest_name);
        fs::create_dir_all(&dest_path)?;

        let file = File::open(&archive_path)?;
        let mut archive = ZipArchive::new(file)?;

        for i in 0..archive.len() {
            let mut file = archive.by_index(i)?;
            let outpath = match file.enclosed_name() {
                Some(path) => dest_path.join(path),
                None => continue,
            };

            if file.is_dir() {
                fs::create_dir_all(&outpath)?;
            } else {
                if let Some(p) = outpath.parent() {
                    if !p.exists() {
                        fs::create_dir_all(p)?;
                    }
                }
                let mut outfile = File::create(&outpath)?;
                std::io::copy(&mut file, &mut outfile)?;
            }
        }

        Ok(dest_path)
    }

    /// Lists all local backup ZIP archives stored in the instance's `backups/` directory.
    pub fn list_local_backups(instance_dir: &Path) -> Result<Vec<BackupArchiveInfo>> {
        let backups_dir = WorldInspector::get_backups_dir(instance_dir);
        if !backups_dir.exists() {
            return Ok(Vec::new());
        }

        let mut backups = Vec::new();
        let entries = fs::read_dir(&backups_dir)?;

        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() && path.extension().and_then(|s| s.to_str()) == Some("zip") {
                let file_name = path.file_name().unwrap().to_string_lossy().to_string();
                let metadata = path.metadata()?;
                let world_name = file_name.split('_').next().unwrap_or(&file_name).to_string();

                backups.push(BackupArchiveInfo {
                    file_name,
                    world_name,
                    timestamp: chrono::Utc::now().to_rfc3339(),
                    size_bytes: metadata.len(),
                    checksum_blake3: String::new(),
                    is_remote: false,
                });
            }
        }

        backups.sort_by(|a, b| b.file_name.cmp(&a.file_name));
        Ok(backups)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_create_and_restore_backup() {
        let tmp = tempdir().unwrap();
        let instance = tmp.path();
        let saves = instance.join("minecraft").join("saves");
        let world = saves.join("HardcoreWorld");
        fs::create_dir_all(&world).unwrap();
        fs::write(world.join("level.dat"), b"level_dat_mock_content").unwrap();

        // 1. Create backup
        let archive_info = WorldArchiver::create_backup(instance, "HardcoreWorld").unwrap();
        assert!(archive_info.file_name.starts_with("HardcoreWorld_"));
        assert!(archive_info.size_bytes > 0);

        // 2. List backups
        let backups = WorldArchiver::list_local_backups(instance).unwrap();
        assert_eq!(backups.len(), 1);

        // 3. Restore backup to new world
        let restored_path = WorldArchiver::restore_backup(
            instance,
            &archive_info.file_name,
            Some("HardcoreWorld_Restored"),
        )
        .unwrap();

        assert!(restored_path.exists());
        assert!(restored_path.join("level.dat").exists());
    }
}
