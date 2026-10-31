//! World save backup / restore helpers built on `launchcraft-backup`.
//!
//! Backups are zipped per world via [`WorldArchiver`]; when the instance's
//! `BackupConfig` allows it (`server_backups_enabled` + endpoint URL), the
//! fresh archives are uploaded to the server via [`BackupSyncClient`].
//! All long-running work (zipping, HTTP) happens on the tokio runtime, never
//! on the UI thread; progress arrives through an mpsc channel.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;

use eframe::egui;
use launchcraft_backup::archiver::WorldArchiver;
use launchcraft_backup::client::BackupSyncClient;
use launchcraft_backup::inspector::WorldInspector;
use launchcraft_core::instance::{BackupArchiveInfo, BackupConfig, InstanceConfig, WorldSaveInfo};

/// A locally installed instance discovered under the launcher `instances/` directory.
pub struct InstanceEntry {
    pub dir: PathBuf,
    pub config: InstanceConfig,
}

impl InstanceEntry {
    pub fn display_name(&self) -> &str {
        &self.config.name
    }

    /// Whether cloud uploads are actually possible: server policy enabled *and* an endpoint set.
    pub fn cloud_upload_possible(&self) -> bool {
        self.config.backup_config.server_backups_enabled
            && self.config.backup_config.endpoint_url.is_some()
    }
}

/// Scan the launcher's `instances/` directory for instances with a readable `instance.json`.
pub fn list_instances(instances_dir: &Path) -> Vec<InstanceEntry> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(instances_dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        let config_path = dir.join("instance.json");
        if !config_path.is_file() {
            continue;
        }
        let Ok(text) = fs::read_to_string(&config_path) else {
            continue;
        };
        if let Ok(config) = serde_json::from_str::<InstanceConfig>(&text) {
            out.push(InstanceEntry { dir, config });
        }
    }
    out.sort_by_key(|entry| entry.config.name.to_lowercase());
    out
}

/// The world saves of an instance, or an empty list if they cannot be scanned.
pub fn worlds(instance_dir: &Path) -> Vec<WorldSaveInfo> {
    WorldInspector::scan_worlds(instance_dir).unwrap_or_default()
}

/// Local backup archives of an instance.
pub fn local_backups(instance_dir: &Path) -> Vec<BackupArchiveInfo> {
    WorldArchiver::list_local_backups(instance_dir).unwrap_or_default()
}

/// Restore a backup archive into the instance's `saves/` directory.
pub fn restore(instance_dir: &Path, archive_name: &str) -> Result<PathBuf, String> {
    WorldArchiver::restore_backup(instance_dir, archive_name, None).map_err(|e| e.to_string())
}

/// Point the instance at a backup server by rewriting `backup_config.endpoint_url`
/// in its `instance.json`. An empty string clears the endpoint.
/// Returns the stored value (`None` when cleared).
pub fn set_endpoint(instance_dir: &Path, endpoint: &str) -> Result<Option<String>, String> {
    let trimmed = endpoint.trim();
    let url = (!trimmed.is_empty()).then(|| trimmed.to_string());
    if let Some(url) = &url {
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return Err("endpoint must start with http:// or https://".into());
        }
    }

    let config_path = instance_dir.join("instance.json");
    let text = fs::read_to_string(&config_path)
        .map_err(|e| format!("could not read instance.json: {e}"))?;
    let mut config: InstanceConfig =
        serde_json::from_str(&text).map_err(|e| format!("could not parse instance.json: {e}"))?;
    config.backup_config.endpoint_url = url.clone();
    let json = serde_json::to_vec_pretty(&config).map_err(|e| e.to_string())?;
    fs::write(&config_path, json).map_err(|e| format!("could not write instance.json: {e}"))?;
    Ok(url)
}

/// Directory holding an instance's local backup archives.
pub fn backups_dir(instance_dir: &Path) -> PathBuf {
    WorldInspector::get_backups_dir(instance_dir)
}

/// A progress line reported by a background backup/upload job.
pub struct JobEvent {
    pub line: String,
    /// Marks the final line of a job; the UI re-enables its buttons on `done`.
    pub done: bool,
    /// Completion fraction 0.0..=1.0 when the job can measure one.
    pub progress: Option<f32>,
}

/// Zip one world (or all worlds, when `world_names` is empty) and upload the
/// fresh archives to the server if the instance's backup policy allows it.
///
/// Runs on the tokio runtime: zipping happens via `spawn_blocking`, uploads
/// are async. Progress lines are sent to `tx` and a repaint is requested on
/// `ctx` after each one.
pub fn spawn_backup_job(
    rt: &tokio::runtime::Runtime,
    ctx: egui::Context,
    tx: Sender<JobEvent>,
    instance_dir: PathBuf,
    config: BackupConfig,
    world_names: Vec<String>,
) {
    rt.spawn(async move {
        let dir = instance_dir.clone();
        let (block_tx, block_ctx) = (tx.clone(), ctx.clone());
        let made: Vec<BackupArchiveInfo> = tokio::task::spawn_blocking(move || {
            let (tx, ctx) = (block_tx, block_ctx);
            let targets: Vec<String> = if world_names.is_empty() {
                worlds(&dir).into_iter().map(|w| w.folder_name).collect()
            } else {
                world_names
            };
            let mut archives = Vec::new();
            for world in targets {
                match WorldArchiver::create_backup(&dir, &world) {
                    Ok(info) => {
                        report(
                            &tx,
                            &ctx,
                            format!("📦 Backed up '{}'", info.file_name),
                            false,
                        );
                        archives.push(info);
                    }
                    Err(e) => report(
                        &tx,
                        &ctx,
                        format!("⚠ Backup of '{world}' failed: {e}"),
                        false,
                    ),
                }
            }
            archives
        })
        .await
        .unwrap_or_default();

        let total = made.len();
        if total == 0 {
            report(&tx, &ctx, "No worlds were backed up.".into(), true);
            return;
        }

        let names: Vec<String> = made.iter().map(|a| a.file_name.clone()).collect();
        let uploaded = upload_archives(&instance_dir, &config, &names, &tx, &ctx).await;
        let endpoint = config.endpoint_url.clone().unwrap_or_default();
        let summary = if uploaded == total {
            format!("Backed up {total} world(s); {uploaded} uploaded to {endpoint}.")
        } else {
            format!(
                "Backed up {total} world(s); {uploaded}/{total} uploaded to {endpoint} — see Logs."
            )
        };
        report(&tx, &ctx, summary, true);
    });
}

/// Upload existing local backup archives to the server, respecting policy.
pub fn spawn_upload_job(
    rt: &tokio::runtime::Runtime,
    ctx: egui::Context,
    tx: Sender<JobEvent>,
    instance_dir: PathBuf,
    config: BackupConfig,
    archive_names: Vec<String>,
) {
    rt.spawn(async move {
        let total = archive_names.len();
        let uploaded = upload_archives(&instance_dir, &config, &archive_names, &tx, &ctx).await;
        let summary = if uploaded == total {
            format!("Uploaded {uploaded}/{total} backup(s) to the server.")
        } else {
            format!("Uploaded {uploaded}/{total} backup(s) — see Logs.")
        };
        report(&tx, &ctx, summary, true);
    });
}

/// Shared upload path for both jobs. Returns the number of successful uploads.
///
/// Never contacts the server when the policy disables cloud backups, matching
/// `BackupSyncClient::upload_backup`'s own enforcement.
async fn upload_archives(
    instance_dir: &Path,
    config: &BackupConfig,
    archive_names: &[String],
    tx: &Sender<JobEvent>,
    ctx: &egui::Context,
) -> usize {
    if !config.server_backups_enabled {
        report(
            tx,
            ctx,
            "Cloud upload skipped: disabled by server policy.".into(),
            false,
        );
        return 0;
    }
    if config.endpoint_url.is_none() {
        report(
            tx,
            ctx,
            "Cloud upload skipped: no backup endpoint configured.".into(),
            false,
        );
        return 0;
    }

    let client = BackupSyncClient::new();
    let mut uploaded = 0usize;
    for name in archive_names {
        match client.upload_backup(instance_dir, config, name, None).await {
            Ok(info) => {
                uploaded += 1;
                report(
                    tx,
                    ctx,
                    format!("☁ Uploaded {} ({})", name, human_size(info.size_bytes)),
                    false,
                );
            }
            Err(e) => report(tx, ctx, format!("⚠ Upload of {name} failed: {e}"), false),
        }
    }
    uploaded
}

pub(crate) fn report(tx: &Sender<JobEvent>, ctx: &egui::Context, line: String, done: bool) {
    let _ = tx.send(JobEvent {
        line,
        done,
        progress: None,
    });
    ctx.request_repaint();
}

/// Like [`report`], but also carries a completion fraction for the progress bar.
pub(crate) fn report_progress(
    tx: &Sender<JobEvent>,
    ctx: &egui::Context,
    line: String,
    progress: Option<f32>,
    done: bool,
) {
    let _ = tx.send(JobEvent {
        line,
        done,
        progress,
    });
    ctx.request_repaint();
}

/// Format a byte count as a short human-readable size.
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0usize;
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};
    use tempfile::tempdir;

    fn instance_json(enabled: bool, endpoint: Option<&str>) -> String {
        let endpoint = match endpoint {
            Some(e) => format!("\"{e}\""),
            None => "null".to_string(),
        };
        format!(
            r#"{{
  "id": "00000000-0000-0000-0000-000000000001",
  "name": "Test Pack",
  "minecraft_version": "1.21.1",
  "jvm_args": [],
  "source": {{ "type": "manual", "created_at": "2026-01-01T00:00:00Z" }},
  "backup_config": {{
    "server_backups_enabled": {enabled},
    "endpoint_url": {endpoint},
    "max_local_backups": 5,
    "auto_backup_on_launch": false
  }}
}}"#
        )
    }

    /// Build an instance dir with one fake world (including a `session.lock`
    /// that must be excluded from the archive).
    fn make_instance(tmp: &Path, json: &str) -> PathBuf {
        let dir = tmp.join("instances").join("pack1");
        let world = dir.join("minecraft").join("saves").join("TestWorld");
        fs::create_dir_all(world.join("region")).unwrap();
        fs::write(world.join("level.dat"), b"level_dat_mock").unwrap();
        fs::write(world.join("region").join("r1.mca"), b"chunk_data").unwrap();
        fs::write(world.join("session.lock"), b"locked").unwrap();
        fs::write(dir.join("instance.json"), json).unwrap();
        dir
    }

    /// One-shot HTTP server capturing `POST /backups/upload` requests.
    fn spawn_mock_backup_server() -> (u16, Arc<Mutex<Vec<String>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen_thread = seen.clone();
        std::thread::spawn(move || {
            let Ok((mut sock, _)) = listener.accept() else {
                return;
            };
            let mut head: Vec<u8> = Vec::new();
            let mut byte = [0u8; 1];
            loop {
                match sock.read(&mut byte) {
                    Ok(1) => {
                        head.push(byte[0]);
                        if head.ends_with(b"\r\n\r\n") {
                            break;
                        }
                    }
                    _ => return,
                }
            }
            let head_text = String::from_utf8_lossy(&head).to_string();
            let body_len: usize = head_text
                .lines()
                .find_map(|l| {
                    let (k, v) = l.split_once(':')?;
                    k.eq_ignore_ascii_case("content-length")
                        .then(|| v.trim().parse().ok())?
                })
                .unwrap_or(0);
            let mut remaining = body_len;
            let mut buf = [0u8; 8192];
            while remaining > 0 {
                let want = remaining.min(buf.len());
                match sock.read(&mut buf[..want]) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => remaining -= n,
                }
            }
            let first_line = head_text.lines().next().unwrap_or_default().to_string();
            seen_thread
                .lock()
                .unwrap()
                .push(format!("{first_line} body_bytes={body_len}"));
            let _ = sock
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
        });
        (port, seen)
    }

    fn drain_until_done(rx: &std::sync::mpsc::Receiver<JobEvent>) -> Vec<String> {
        let mut lines = Vec::new();
        loop {
            let ev = rx.recv_timeout(std::time::Duration::from_secs(30)).unwrap();
            lines.push(ev.line.clone());
            if ev.done {
                return lines;
            }
        }
    }

    #[test]
    fn backup_zips_world_and_uploads_it_to_server() {
        let tmp = tempdir().unwrap();
        let (port, seen) = spawn_mock_backup_server();
        let json = instance_json(true, Some(&format!("http://127.0.0.1:{port}")));
        let dir = make_instance(tmp.path(), &json);
        let config: BackupConfig = serde_json::from_str::<InstanceConfig>(&json)
            .unwrap()
            .backup_config;

        let rt = tokio::runtime::Runtime::new().unwrap();
        let ctx = egui::Context::default();
        let (tx, rx) = std::sync::mpsc::channel();

        spawn_backup_job(&rt, ctx, tx, dir.clone(), config, vec![]);
        let lines = drain_until_done(&rx);

        assert!(
            lines
                .iter()
                .any(|l| l.contains("Backed up 1 world(s); 1 uploaded")),
            "unexpected progress: {lines:?}"
        );

        // The server got exactly one upload of the fresh archive with a body.
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1, "server requests: {seen:?}");
        assert!(seen[0].starts_with("POST /backups/upload?filename=TestWorld_"));
        assert!(!seen[0].ends_with("body_bytes=0"), "upload body was empty");
        drop(seen);

        // The local zip exists and excludes the session lock.
        let backups = local_backups(&dir);
        assert_eq!(backups.len(), 1);
        let zip_file = fs::File::open(backups_dir(&dir).join(&backups[0].file_name)).unwrap();
        let mut archive = zip::ZipArchive::new(zip_file).unwrap();
        let names: Vec<String> = (0..archive.len())
            .map(|i| archive.by_index(i).unwrap().name().to_string())
            .collect();
        assert!(names.iter().any(|n| n.ends_with("level.dat")));
        assert!(names.iter().any(|n| n.ends_with("region/r1.mca")));
        assert!(
            !names.iter().any(|n| n.contains("session.lock")),
            "session.lock must not be backed up: {names:?}"
        );
    }

    #[test]
    fn backup_stays_local_when_policy_disabled() {
        let tmp = tempdir().unwrap();
        let (port, seen) = spawn_mock_backup_server();
        let json = instance_json(false, Some(&format!("http://127.0.0.1:{port}")));
        let dir = make_instance(tmp.path(), &json);
        let config: BackupConfig = serde_json::from_str::<InstanceConfig>(&json)
            .unwrap()
            .backup_config;

        let rt = tokio::runtime::Runtime::new().unwrap();
        let ctx = egui::Context::default();
        let (tx, rx) = std::sync::mpsc::channel();

        spawn_backup_job(&rt, ctx, tx, dir.clone(), config, vec![]);
        let lines = drain_until_done(&rx);

        assert!(
            lines
                .iter()
                .any(|l| l.contains("disabled by server policy")),
            "expected policy skip: {lines:?}"
        );
        assert!(
            seen.lock().unwrap().is_empty(),
            "server must not be contacted"
        );
        assert_eq!(local_backups(&dir).len(), 1, "local backup still created");
    }

    #[test]
    fn set_endpoint_round_trips_instance_json() {
        let tmp = tempdir().unwrap();
        let dir = make_instance(
            tmp.path(),
            &instance_json(true, Some("http://old.example/api")),
        );
        let read_config = || -> InstanceConfig {
            let text = fs::read_to_string(dir.join("instance.json")).unwrap();
            serde_json::from_str(&text).unwrap()
        };

        // Set a new endpoint: value is trimmed, stored, and the policy flag is untouched.
        let stored = set_endpoint(&dir, " https://new.example.com/api ").unwrap();
        assert_eq!(stored.as_deref(), Some("https://new.example.com/api"));
        let cfg = read_config();
        assert_eq!(
            cfg.backup_config.endpoint_url.as_deref(),
            Some("https://new.example.com/api")
        );
        assert!(cfg.backup_config.server_backups_enabled);
        assert_eq!(cfg.name, "Test Pack");

        // Clearing works.
        assert_eq!(set_endpoint(&dir, "").unwrap(), None);
        assert_eq!(read_config().backup_config.endpoint_url, None);

        // Non-http endpoints are rejected and leave the file unchanged.
        assert!(set_endpoint(&dir, "ftp://nope").is_err());
        assert_eq!(read_config().backup_config.endpoint_url, None);
    }
}
