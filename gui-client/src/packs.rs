//! SKCraft-style pack browser: pulls the server's built-pack list, shows
//! per-pack install state, and installs/updates instances via `launchcraft-sync`.

use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::sync::Arc;

use eframe::egui;
use launchcraft_core::instance::{InstanceConfig, InstanceSource, RepositoryConfig};
use launchcraft_core::paths::LauncherPaths;
use launchcraft_sync::{CasStore, SyncEngine};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::saves::{report, report_progress, JobEvent};

/// One row from the server's public `GET /packs` list.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PackInfo {
    pub id: String,
    pub title: String,
    pub game_version: String,
    pub pack_version: String,
    #[serde(default)]
    pub loader: Option<String>,
    #[serde(default)]
    pub file_count: usize,
    #[serde(default)]
    pub total_bytes: u64,
}

/// Outcome of `GET /packs`.
#[derive(Deserialize)]
struct PackList {
    #[serde(default)]
    packs: Vec<PackInfo>,
}

/// How a server pack relates to the local instances (SKCraft's list badges).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackState {
    NotInstalled,
    Installed,
    UpdateAvailable,
}

impl PackState {
    pub fn label(self) -> &'static str {
        match self {
            PackState::NotInstalled => "not installed",
            PackState::Installed => "installed",
            PackState::UpdateAvailable => "requires update",
        }
    }
}

pub fn classify(server_version: &str, local_version: Option<&str>) -> PackState {
    match local_version {
        None => PackState::NotInstalled,
        Some(v) if v == server_version => PackState::Installed,
        Some(_) => PackState::UpdateAvailable,
    }
}

/// The locally installed `pack_version` for `pack_id`, if any instance syncs it.
pub fn local_pack_version(instances_dir: &Path, pack_id: &str) -> Option<String> {
    let entries = std::fs::read_dir(instances_dir).ok()?;
    for entry in entries.flatten() {
        let Ok(text) = std::fs::read_to_string(entry.path().join("instance.json")) else {
            continue;
        };
        let Ok(config) = serde_json::from_str::<InstanceConfig>(&text) else {
            continue;
        };
        if let InstanceSource::RemoteSync {
            pack_id: id,
            version,
            ..
        } = config.source
        {
            if id == pack_id {
                return Some(version);
            }
        }
    }
    None
}

/// Client-side settings kept apart from the Java settings file so neither
/// save path clobbers the other. (`<config>/client.json`)
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ClientSettings {
    #[serde(default)]
    pub pack_server_url: Option<String>,
}

impl ClientSettings {
    fn file(config_dir: &Path) -> PathBuf {
        config_dir.join("client.json")
    }

    pub fn load(config_dir: &Path) -> Self {
        std::fs::read(Self::file(config_dir))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, config_dir: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(config_dir)?;
        std::fs::write(Self::file(config_dir), serde_json::to_vec_pretty(self)?)
    }
}

/// Compact, human-readable byte count for the pack details pane.
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn base_url(url: &str) -> String {
    url.trim().trim_end_matches('/').to_string()
}

/// Reject pack ids that could escape the instances directory.
fn safe_pack_id(id: &str) -> Result<(), String> {
    let ok = !id.is_empty()
        && id.len() <= 64
        && !id.starts_with('.')
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'));
    if ok {
        Ok(())
    } else {
        Err(format!("unsafe pack id from server: {id:?}"))
    }
}

/// Fetch the installable pack list from a launchcraft-server base URL.
pub async fn fetch_packs(url: &str) -> Result<Vec<PackInfo>, String> {
    let list: PackList = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .map_err(|e| e.to_string())?
        .get(format!("{}/packs", base_url(url)))
        .send()
        .await
        .map_err(|e| format!("could not reach server: {e}"))?
        .error_for_status()
        .map_err(|e| format!("server error: {e}"))?
        .json()
        .await
        .map_err(|e| format!("bad pack list: {e}"))?;
    Ok(list.packs)
}

/// Kick off a background pack-list fetch; the result lands in `tx` and the UI
/// repaints when it arrives.
pub fn spawn_fetch(
    rt: &tokio::runtime::Runtime,
    ctx: egui::Context,
    tx: Sender<Result<Vec<PackInfo>, String>>,
    url: String,
) {
    rt.spawn(async move {
        let result = fetch_packs(&url).await;
        let _ = tx.send(result);
        ctx.request_repaint();
    });
}

/// Create (or refresh) the local `instance.json` for a remote pack install.
pub fn prepare_instance(instance_dir: &Path, pack: &PackInfo) -> Result<InstanceConfig, String> {
    safe_pack_id(&pack.id)?;
    let game_dir = instance_dir.join("minecraft");
    std::fs::create_dir_all(&game_dir).map_err(|e| e.to_string())?;

    let config_path = instance_dir.join("instance.json");
    let mut config = std::fs::read(&config_path)
        .ok()
        .and_then(|b| serde_json::from_slice::<InstanceConfig>(&b).ok())
        .unwrap_or_else(|| InstanceConfig {
            id: Uuid::new_v4(),
            name: pack.title.clone(),
            minecraft_version: pack.game_version.clone(),
            mod_loader: None,
            java_path: None,
            jvm_args: Vec::new(),
            source: InstanceSource::Manual {
                created_at: chrono::Utc::now(),
            },
            icon_path: None,
            backup_config: Default::default(),
        });
    config.name = pack.title.clone();
    config.minecraft_version = pack.game_version.clone();
    config.source = InstanceSource::RemoteSync {
        repo_url: String::new(), // filled by install_repo_url(); stored for display only
        pack_id: pack.id.clone(),
        version: String::new(),
        etag: None,
        locked: false,
    };
    std::fs::write(
        &config_path,
        serde_json::to_vec_pretty(&config).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    Ok(config)
}

fn install_repo_url(server_url: &str, pack_id: &str) -> String {
    format!("{}/packs/{}", base_url(server_url), pack_id)
}

/// Record a successful sync of `version` into the instance metadata.
fn mark_installed(
    instance_dir: &Path,
    repo_url: &str,
    pack: &PackInfo,
    version: &str,
) -> Result<(), String> {
    let config_path = instance_dir.join("instance.json");
    let text = std::fs::read_to_string(&config_path).map_err(|e| e.to_string())?;
    let mut config: InstanceConfig = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    config.source = InstanceSource::RemoteSync {
        repo_url: repo_url.to_string(),
        pack_id: pack.id.clone(),
        version: version.to_string(),
        etag: None,
        locked: false,
    };
    std::fs::write(
        &config_path,
        serde_json::to_vec_pretty(&config).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())
}

/// Download and sync a pack into `<instances>/<id>/minecraft/`, streaming
/// progress into the log channel. Never blocks the UI thread.
pub fn spawn_install(
    rt: &tokio::runtime::Runtime,
    ctx: egui::Context,
    tx: Sender<JobEvent>,
    paths: LauncherPaths,
    server_url: String,
    pack: PackInfo,
) {
    rt.spawn(async move {
        match do_install(&paths, &server_url, &pack, &tx, &ctx).await {
            Ok(version) => report_progress(
                &tx,
                &ctx,
                format!("Installed '{}' v{version} into {}.", pack.title, pack.id),
                Some(1.0),
                true,
            ),
            Err(e) => report(
                &tx,
                &ctx,
                format!("Install of '{}' failed: {e}", pack.title),
                true,
            ),
        }
    });
}

async fn do_install(
    paths: &LauncherPaths,
    server_url: &str,
    pack: &PackInfo,
    tx: &Sender<JobEvent>,
    ctx: &egui::Context,
) -> Result<String, String> {
    let instance_dir = paths.instances.join(&pack.id);
    prepare_instance(&instance_dir, pack)?;
    let repo_url = install_repo_url(server_url, &pack.id);

    report_progress(
        tx,
        ctx,
        format!("Fetching manifest for '{}'…", pack.id),
        Some(0.0),
        false,
    );
    let engine = SyncEngine::new(Arc::new(CasStore::new(&paths.cas_cache)));
    let manifest = engine
        .fetch_remote_manifest(&RepositoryConfig {
            url: repo_url.clone(),
            etag: None,
        })
        .await
        .map_err(|e| e.to_string())?;

    let (tx2, ctx2) = (tx.clone(), ctx.clone());
    let last_sent = std::sync::Mutex::new(-1.0f32);
    engine
        .synchronize(
            &repo_url,
            &instance_dir.join("minecraft"),
            &manifest,
            Some(Arc::new(move |p| {
                let frac = sync_fraction(&p);
                let done = p.files_total > 0 && p.files_completed == p.files_total;
                let mut last = last_sent.lock().unwrap_or_else(|e| e.into_inner());
                // Throttle: one event per ~0.5 % of progress, plus the final one.
                if done || frac >= *last + 0.005 {
                    *last = frac;
                    report_progress(
                        &tx2,
                        &ctx2,
                        format!(
                            "⬇ {:.0}% — {}/{} files — {}",
                            frac * 100.0,
                            p.files_completed,
                            p.files_total,
                            p.current_operation
                        ),
                        Some(frac),
                        false,
                    );
                }
            })),
        )
        .await
        .map_err(|e| e.to_string())?;

    mark_installed(&instance_dir, &repo_url, pack, &manifest.pack_version)?;
    Ok(manifest.pack_version)
}

/// Completion fraction of a `SyncEngine` progress report: bytes when the pack
/// knows them (the usual case), falling back to a file count.
fn sync_fraction(p: &launchcraft_sync::SyncProgress) -> f32 {
    if p.bytes_total > 0 {
        (p.bytes_downloaded as f64 / p.bytes_total as f64).min(1.0) as f32
    } else if p.files_total > 0 {
        (p.files_completed as f32 / p.files_total as f32).min(1.0)
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_matches_skcraft_states() {
        assert_eq!(classify("1.0.0", None), PackState::NotInstalled);
        assert_eq!(classify("1.0.0", Some("1.0.0")), PackState::Installed);
        assert_eq!(classify("1.1.0", Some("1.0.0")), PackState::UpdateAvailable);
    }

    #[test]
    fn settings_roundtrip_keeps_java_file_untouched() {
        let tmp = tempfile::tempdir().unwrap();
        let s = ClientSettings {
            pack_server_url: Some("http://localhost:9090".into()),
        };
        s.save(tmp.path()).unwrap();
        assert_eq!(
            ClientSettings::load(tmp.path()).pack_server_url.as_deref(),
            Some("http://localhost:9090")
        );
        assert!(!tmp.path().join("settings.json").exists());
    }

    fn write_instance(dir: &Path, id: &str, pack_id: &str, version: &str) {
        let inst = dir.join(id);
        std::fs::create_dir_all(&inst).unwrap();
        let config = InstanceConfig {
            id: Uuid::new_v4(),
            name: id.into(),
            minecraft_version: "1.21.4".into(),
            mod_loader: None,
            java_path: None,
            jvm_args: Vec::new(),
            source: InstanceSource::RemoteSync {
                repo_url: "http://x/packs".into(),
                pack_id: pack_id.into(),
                version: version.into(),
                etag: None,
                locked: false,
            },
            icon_path: None,
            backup_config: Default::default(),
        };
        std::fs::write(
            inst.join("instance.json"),
            serde_json::to_vec(&config).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn local_pack_version_finds_remote_sync_instances() {
        let tmp = tempfile::tempdir().unwrap();
        write_instance(tmp.path(), "alice", "alice-1.7", "1.0.0");
        assert_eq!(
            local_pack_version(tmp.path(), "alice-1.7").as_deref(),
            Some("1.0.0")
        );
        assert_eq!(local_pack_version(tmp.path(), "other"), None);
    }

    #[test]
    fn prepare_instance_rejects_unsafe_ids() {
        let tmp = tempfile::tempdir().unwrap();
        let pack = PackInfo {
            id: "../evil".into(),
            title: "Evil".into(),
            game_version: "1.0".into(),
            pack_version: "1".into(),
            loader: None,
            file_count: 0,
            total_bytes: 0,
        };
        assert!(prepare_instance(&tmp.path().join("x"), &pack).is_err());
    }

    #[test]
    fn fetch_packs_parses_server_list() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            use std::io::{Read, Write};
            let mut buf = [0u8; 1024];
            let _ = sock.read(&mut buf).unwrap();
            let body = r#"{"packs":[{"id":"alice","title":"Alice 1.7","game_version":"1.7.10","pack_version":"1.0.0","file_count":3,"total_bytes":1400}]}"#;
            let _ = sock.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
                .as_bytes(),
            );
        });
        let rt = tokio::runtime::Runtime::new().unwrap();
        let packs = rt
            .block_on(fetch_packs(&format!("http://127.0.0.1:{port}/")))
            .unwrap();
        assert_eq!(packs.len(), 1);
        assert_eq!(packs[0].id, "alice");
        assert_eq!(packs[0].game_version, "1.7.10");
    }
}
