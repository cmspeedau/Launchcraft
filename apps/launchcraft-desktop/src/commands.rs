//! Desktop IPC commands invoked from the frontend shell.

use std::path::PathBuf;
use serde::{Deserialize, Serialize};
use launchcraft_core::instance::InstanceConfig;
use launchcraft_auth::types::{DeviceCodeResponse, MinecraftAccount};
use launchcraft_auth::MinecraftAuthenticator;
use crate::state::AppState;

#[derive(Debug, Serialize, Deserialize)]
pub struct SystemInfoResponse {
    pub is_portable: bool,
    pub root_dir: PathBuf,
    pub os: String,
    pub arch: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct InstanceSummary {
    pub id: String,
    pub title: String,
    pub version: String,
    pub memory_max_mb: u32,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct LaunchLogEntry {
    pub timestamp: String,
    pub stream: String,
    pub line: String,
}

/// Retrieve information about launcher paths and portable mode.
pub async fn get_system_info(state: &AppState) -> Result<SystemInfoResponse, String> {
    Ok(SystemInfoResponse {
        is_portable: state.paths.is_portable(),
        root_dir: state.paths.root().to_path_buf(),
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
    })
}

/// List all installed modpack instances.
pub async fn list_instances(state: &AppState) -> Result<Vec<InstanceSummary>, String> {
    let instances_dir = state.paths.instances_dir();
    let mut result = Vec::new();

    if !instances_dir.exists() {
        return Ok(result);
    }

    let entries = std::fs::read_dir(&instances_dir).map_err(|e| e.to_string())?;
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_dir() {
            let config_path = p.join("instance.json");
            if config_path.exists() {
                if let Ok(data) = std::fs::read_to_string(&config_path) {
                    if let Ok(cfg) = serde_json::from_str::<InstanceConfig>(&data) {
                        result.push(InstanceSummary {
                            id: cfg.id,
                            title: cfg.title,
                            version: cfg.version,
                            memory_max_mb: cfg.memory_max_mb,
                        });
                    }
                }
            }
        }
    }

    Ok(result)
}

/// Start Microsoft OAuth Device Code login.
pub async fn auth_start_device_code() -> Result<DeviceCodeResponse, String> {
    let client = MinecraftAuthenticator::new("00000000402b5328");
    client.request_device_code().await.map_err(|e| e.to_string())
}

/// Complete Microsoft OAuth login via device code.
pub async fn auth_poll_device_code(
    state: &AppState,
    device_code: String,
) -> Result<MinecraftAccount, String> {
    let client = MinecraftAuthenticator::new("00000000402b5328");
    let account = client.authenticate_device_code(&device_code).await.map_err(|e| e.to_string())?;
    
    let mut storage = state.accounts.lock().await;
    storage.save_account(account.clone()).map_err(|e| e.to_string())?;
    
    Ok(account)
}

/// Retrieve currently authenticated accounts.
pub async fn get_accounts(state: &AppState) -> Result<Vec<MinecraftAccount>, String> {
    let mut storage = state.accounts.lock().await;
    let _ = storage.load();
    Ok(storage.accounts().to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn test_desktop_system_info_and_instances() {
        let tmp = tempdir().unwrap();
        let paths = LauncherPaths::new(tmp.path().to_path_buf(), true);
        let state = AppState::new(paths);

        let info = get_system_info(&state).await.unwrap();
        assert!(info.is_portable);
        assert_eq!(info.root_dir, tmp.path());

        let instances = list_instances(&state).await.unwrap();
        assert!(instances.is_empty());
    }
}
