//! World save backup / restore helpers.

use std::path::{Path, PathBuf};

/// Directory where world backups are stored.
pub fn backup_dir(settings_dir: &Path) -> PathBuf {
    settings_dir.join("backups")
}

/// Default Minecraft saves directory for the current OS.
pub fn saves_dir() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        let appdata = std::env::var("APPDATA").unwrap_or_default();
        PathBuf::from(appdata).join(".minecraft").join("saves")
    }
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var("HOME").unwrap_or_default();
        PathBuf::from(home).join("Library/Application Support/minecraft/saves")
    }
    #[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
    {
        let home = std::env::var("HOME").unwrap_or_default();
        PathBuf::from(home).join(".minecraft/saves")
    }
}

/// Back up every world in the saves directory into a timestamped folder.
pub fn backup_all(settings_dir: &Path) -> anyhow_lite(settings_dir) {
    unimplemented!()
}

// NOTE: keep signature simple without extra deps
pub type BackupResult = Result<usize, String>;

fn anyhow_lite(settings_dir: &Path) -> BackupResult {
    backup_all_impl(settings_dir)
}

fn backup_all_impl(settings_dir: &Path) -> BackupResult {
    let saves = saves_dir();
    let stamp = chrono::Local::now().format("%Y-%m-%d_%H%M%S").to_string();
    let dest_root = backup_dir(settings_dir).join(&stamp);
    std::fs::create_dir_all(&dest_root).map_err(|e| e.to_string())?;

    let mut count = 0usize;
    let entries = std::fs::read_dir(&saves).map_err(|e| e.to_string())?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let name = entry.file_name();
            let dest = dest_root.join(&name);
            std::fs::create_dir_all(&dest).map_err(|e| e.to_string())?;
            copy_dir(&path, &dest).map_err(|e| e.to_string())?;
            count += 1;
        }
    }
    Ok(count)
}

/// Recursively copy a directory.
pub fn copy_dir(src: &Path, dest: &Path) -> Result<(), String> {
    fs_extra::dir::copy(src, dest, &fs_extra::dir::CopyOptions::new().copy_inside(true))
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// Restore a backup folder (named by timestamp) back into the saves directory.
pub fn restore(settings_dir: &Path, backup_name: &str) -> Result<(), String> {
    let src = backup_dir(settings_dir).join(backup_name);
    let saves = saves_dir();
    let entries = std::fs::read_dir(&src).map_err(|e| e.to_string())?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let dest = saves.join(entry.file_name());
            std::fs::create_dir_all(&dest).map_err(|e| e.to_string())?;
            copy_dir(&path, &dest)?;
        }
    }
    Ok(())
}
