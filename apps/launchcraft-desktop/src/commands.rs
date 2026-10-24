//! Desktop IPC commands invoked from the frontend shell.

use std::path::PathBuf;
use serde::{Deserialize, Serialize};
use launchcraft_core::instance::{BackupArchiveInfo, InstanceConfig, WorldSaveInfo};
use launchcraft_auth::{DeviceCodeResponse, LaunchcraftAccount, MicrosoftAuthClient};
use launchcraft_backup::archiver::WorldArchiver;
use launchcraft_backup::client::BackupSyncClient;
use launchcraft_backup::inspector::WorldInspector;
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
    pub name: String,
    pub minecraft_version: String,
    pub server_backups_enabled: bool,
}

/// Retrieve information about launcher paths and portable mode.
pub async fn get_system_info(state: &AppState) -> Result<SystemInfoResponse, String> {
    Ok(SystemInfoResponse {
        is_portable: state.paths.is_portable,
        root_dir: state.paths.root.clone(),
        os: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
    })
}

/// List all installed modpack instances.
pub async fn list_instances(state: &AppState) -> Result<Vec<InstanceSummary>, String> {
    let instances_dir = state.paths.instances.clone();
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
                            id: cfg.id.to_string(),
                            name: cfg.name,
                            minecraft_version: cfg.minecraft_version,
                            server_backups_enabled: cfg.backup_config.server_backups_enabled,
                        });
                    }
                }
            }
        }
    }

    Ok(result)
}

/// Lists all singleplayer world saves for a specific instance.
pub async fn list_instance_worlds(
    state: &AppState,
    instance_id: String,
) -> Result<Vec<WorldSaveInfo>, String> {
    let instance_dir = state.paths.instances.join(&instance_id);
    WorldInspector::scan_worlds(&instance_dir).map_err(|e| e.to_string())
}

/// Creates a zipped backup of a target world.
pub async fn create_world_backup(
    state: &AppState,
    instance_id: String,
    world_folder_name: String,
) -> Result<BackupArchiveInfo, String> {
    let instance_dir = state.paths.instances.join(&instance_id);
    WorldArchiver::create_backup(&instance_dir, &world_folder_name).map_err(|e| e.to_string())
}

/// Restores a zipped backup archive into the instance saves directory.
pub async fn restore_world_backup(
    state: &AppState,
    instance_id: String,
    archive_name: String,
    target_world_name: Option<String>,
) -> Result<String, String> {
    let instance_dir = state.paths.instances.join(&instance_id);
    let path = WorldArchiver::restore_backup(
        &instance_dir,
        &archive_name,
        target_world_name.as_deref(),
    )
    .map_err(|e| e.to_string())?;
    Ok(path.to_string_lossy().to_string())
}

/// Lists local backup ZIP files for an instance.
pub async fn list_instance_backups(
    state: &AppState,
    instance_id: String,
) -> Result<Vec<BackupArchiveInfo>, String> {
    let instance_dir = state.paths.instances.join(&instance_id);
    WorldArchiver::list_local_backups(&instance_dir).map_err(|e| e.to_string())
}

/// Uploads a zipped world backup to the remote server if allowed by policy.
pub async fn upload_world_backup(
    state: &AppState,
    instance_id: String,
    archive_name: String,
) -> Result<BackupArchiveInfo, String> {
    let instance_dir = state.paths.instances.join(&instance_id);
    let config_path = instance_dir.join("instance.json");
    let cfg_str = std::fs::read_to_string(&config_path).map_err(|e| e.to_string())?;
    let instance_cfg: InstanceConfig = serde_json::from_str(&cfg_str).map_err(|e| e.to_string())?;

    let client = BackupSyncClient::new();
    client
        .upload_backup(&instance_dir, &instance_cfg.backup_config, &archive_name, None)
        .await
        .map_err(|e| e.to_string())
}

/// Downloads a world ZIP archive from the server.
pub async fn download_world_backup(
    state: &AppState,
    instance_id: String,
    archive_name: String,
) -> Result<BackupArchiveInfo, String> {
    let instance_dir = state.paths.instances.join(&instance_id);
    let config_path = instance_dir.join("instance.json");
    let cfg_str = std::fs::read_to_string(&config_path).map_err(|e| e.to_string())?;
    let instance_cfg: InstanceConfig = serde_json::from_str(&cfg_str).map_err(|e| e.to_string())?;

    let client = BackupSyncClient::new();
    client
        .download_backup(&instance_dir, &instance_cfg.backup_config, &archive_name, None)
        .await
        .map_err(|e| e.to_string())
}

/// Start Microsoft OAuth Device Code login.
pub async fn auth_start_device_code() -> Result<DeviceCodeResponse, String> {
    let client = MicrosoftAuthClient::new(None);
    client
        .initiate_device_flow()
        .await
        .map_err(|e| e.to_string())
}

/// Complete Microsoft OAuth login via device code.
pub async fn auth_poll_device_code(
    state: &AppState,
    device_code: String,
) -> Result<LaunchcraftAccount, String> {
    let client = MicrosoftAuthClient::new(None);
    let (ms_access_token, refresh_token) = client
        .poll_device_code_token(&device_code)
        .await
        .map_err(|e| e.to_string())?;
    let account = client
        .authenticate_minecraft(&ms_access_token, &refresh_token)
        .await
        .map_err(|e| e.to_string())?;

    let storage = state.accounts.lock().await;
    storage.save_account(account.clone()).map_err(|e| e.to_string())?;

    Ok(account)
}

/// Retrieve currently authenticated accounts.
pub async fn get_accounts(state: &AppState) -> Result<Vec<LaunchcraftAccount>, String> {
    let storage = state.accounts.lock().await;
    storage.list_accounts().map_err(|e| e.to_string())
}
