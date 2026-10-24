//! Scans and inspects Minecraft save directories.

use std::fs;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;
use launchcraft_core::instance::WorldSaveInfo;
use crate::{BackupError, Result};

/// World inspector service for scanning singleplayer worlds.
pub struct WorldInspector;

impl WorldInspector {
    /// Scans the given instance root for worlds located in `minecraft/saves/`.
    pub fn scan_worlds(instance_dir: &Path) -> Result<Vec<WorldSaveInfo>> {
        let saves_dir = Self::get_saves_dir(instance_dir);
        if !saves_dir.exists() {
            return Ok(Vec::new());
        }

        let mut worlds = Vec::new();
        let entries = fs::read_dir(&saves_dir)?;

        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if let Some(world_info) = Self::inspect_world(&path) {
                    worlds.push(world_info);
                }
            }
        }

        worlds.sort_by(|a, b| b.last_played_timestamp.cmp(&a.last_played_timestamp));
        Ok(worlds)
    }

    /// Resolves the instance's Minecraft `saves/` folder.
    pub fn get_saves_dir(instance_dir: &Path) -> PathBuf {
        let direct_saves = instance_dir.join("minecraft").join("saves");
        if direct_saves.exists() {
            direct_saves
        } else {
            instance_dir.join("saves")
        }
    }

    /// Resolves the instance's `backups/` directory.
    pub fn get_backups_dir(instance_dir: &Path) -> PathBuf {
        instance_dir.join("backups")
    }

    /// Inspects a specific world directory.
    pub fn inspect_world(world_dir: &Path) -> Option<WorldSaveInfo> {
        let folder_name = world_dir.file_name()?.to_string_lossy().to_string();
        let level_dat = world_dir.join("level.dat");

        // Compute total folder size
        let mut total_size = 0u64;
        let mut latest_mod_time = 0u64;

        for entry in WalkDir::new(world_dir).into_iter().flatten() {
            if entry.file_type().is_file() {
                if let Ok(metadata) = entry.metadata() {
                    total_size += metadata.len();
                    if let Ok(mod_time) = metadata.modified() {
                        let secs = mod_time
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs();
                        if secs > latest_mod_time {
                            latest_mod_time = secs;
                        }
                    }
                }
            }
        }

        let session_lock = world_dir.join("session.lock");
        let is_locked = session_lock.exists();

        Some(WorldSaveInfo {
            folder_name: folder_name.clone(),
            display_name: folder_name,
            last_played_timestamp: latest_mod_time,
            size_bytes: total_size,
            is_locked,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_inspect_and_scan_worlds() {
        let tmp = tempdir().unwrap();
        let instance_dir = tmp.path();
        let saves_dir = instance_dir.join("minecraft").join("saves");
        fs::create_dir_all(&saves_dir).unwrap();

        let world_a = saves_dir.join("Survival_1");
        fs::create_dir_all(&world_a).unwrap();
        fs::write(world_a.join("level.dat"), b"level data").unwrap();

        let worlds = WorldInspector::scan_worlds(instance_dir).unwrap();
        assert_eq!(worlds.len(), 1);
        assert_eq!(worlds[0].folder_name, "Survival_1");
        assert!(worlds[0].size_bytes > 0);
    }
}
