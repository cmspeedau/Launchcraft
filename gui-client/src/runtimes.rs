//! Server-distributed Minecraft runtimes, client side (Phase 11).
//!
//! A launchcraft-server can stage the whole runtime a pack needs — version
//! profiles, client jar, asset index, libraries — into one content-addressed
//! store and publish it over `/runtimes/<id>`. This module installs such a
//! runtime into the launcher's data root so **PLAY** finds
//! `versions/<id>/<id>.json` + `<id>.jar` (and the libraries and asset index
//! beside it) without the user visiting the Versions tab, and without a second
//! copy of the same client jar per pack.
//!
//! ```text
//! <data>/versions/<id>/<id>.json      <data>/assets/indexes/<n>.json
//! <data>/versions/<id>/<id>.jar       <data>/libraries/<maven path>
//! ```
//!
//! Two deliberate differences from a pack sync:
//!
//! * **Additive, never destructive.** A runtime lives in the *shared* data
//!   root, which also holds `instances/`, `config.json` and the local CAS.
//!   Running the usual extraneous-file sweep there would delete the user's
//!   launcher, so only files the manifest names are written; nothing outside
//!   the runtime's own three trees is ever removed. Launchers do not prune
//!   `libraries/` either — another pack may still need the jar.
//! * **Only `versions/`, `assets/` and `libraries/` are reachable.** Every
//!   manifest path is checked against that allowlist before anything is
//!   written, so a hostile or broken server cannot plant `../../config.json`
//!   or overwrite an instance.

use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;

use eframe::egui;
use launchcraft_core::manifest::PackManifest;
use launchcraft_core::paths::LauncherPaths;
use launchcraft_sync::{CasDownloader, CasStore, SyncAction};
use serde::{Deserialize, Serialize};

use crate::saves::{report, report_progress, JobEvent};

/// The only trees a runtime manifest may write into the data root.
pub const RUNTIME_PREFIXES: [&str; 3] = ["versions/", "assets/", "libraries/"];

/// Concurrency for runtime blob fetches: jars are large but few.
const DOWNLOAD_CONCURRENCY: usize = 8;

// ------------------------------------------------------------------ urls

fn base_url(url: &str) -> String {
    url.trim().trim_end_matches('/').to_string()
}

/// Repo-style base for one runtime: `<server>/runtimes/<id>`, which is exactly
/// what `CasStore::remote_object_url` appends `cas/objects/…` to.
pub fn runtime_repo_url(server_url: &str, id: &str) -> String {
    format!("{}/runtimes/{}", base_url(server_url), id)
}

/// Reject runtime ids that could escape the data root or the URL path.
pub fn safe_runtime_id(id: &str) -> Result<(), String> {
    let ok = !id.is_empty()
        && id.len() <= 200
        && !id.starts_with('.')
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '+'));
    if ok {
        Ok(())
    } else {
        Err(format!("unsafe runtime id from server: {id:?}"))
    }
}

/// Normalise and allowlist one manifest path. Returns the sanitized path only
/// when it stays inside one of the three runtime trees.
pub fn safe_runtime_path(raw: &str) -> Option<String> {
    let normalized = raw.replace('\\', "/");
    if normalized.is_empty() || normalized.starts_with('/') {
        return None;
    }
    if !RUNTIME_PREFIXES.iter().any(|p| normalized.starts_with(p)) {
        return None;
    }
    // No empty, dot or parent segments — that also covers "../" and "//".
    normalized
        .split('/')
        .all(|part| !part.is_empty() && part != "." && part != "..")
        .then_some(normalized)
}

// ---------------------------------------------------------------- registry

/// One runtime the launcher already installed, so a re-sync can be skipped.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct InstalledRuntime {
    pub id: String,
    /// The `versions/<id>` entry this runtime installs.
    pub install_version: String,
    pub server_url: String,
    pub file_count: u64,
    pub total_bytes: u64,
    /// BLAKE3 of the manifest that was applied, to spot a rebuilt runtime.
    #[serde(default)]
    pub manifest_hash: String,
    pub installed_at: String,
}

/// `<config>/runtimes.json` — kept out of `client.json` so neither save path
/// clobbers the other, the same split `ClientSettings` uses for Java settings.
#[derive(Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct RuntimeRegistry {
    #[serde(default)]
    pub runtimes: Vec<InstalledRuntime>,
}

impl RuntimeRegistry {
    fn file(config_dir: &Path) -> PathBuf {
        config_dir.join("runtimes.json")
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

    pub fn find(&self, id: &str) -> Option<&InstalledRuntime> {
        self.runtimes.iter().find(|r| r.id == id)
    }

    pub fn record(&mut self, entry: InstalledRuntime) {
        match self.runtimes.iter_mut().find(|r| r.id == entry.id) {
            Some(existing) => *existing = entry,
            None => self.runtimes.push(entry),
        }
        self.runtimes.sort_by(|a, b| a.id.cmp(&b.id));
    }
}

// ------------------------------------------------------------------ install

/// What one install is going to do.
#[derive(Debug, Default, PartialEq)]
pub struct RuntimePlan {
    pub actions: Vec<SyncAction>,
    /// Manifest entries refused by the path allowlist.
    pub refused: Vec<String>,
    pub download_bytes: u64,
    pub download_files: usize,
    pub unchanged_files: usize,
}

/// Decide, per manifest entry, whether the data root already has the exact
/// bytes. Existing files are verified by size then BLAKE3, so a truncated or
/// tampered jar is re-fetched rather than trusted.
pub fn plan_install(root: &Path, manifest: &PackManifest) -> RuntimePlan {
    let mut plan = RuntimePlan::default();
    for (raw, entry) in &manifest.files {
        let Some(rel) = safe_runtime_path(raw) else {
            plan.refused.push(raw.clone());
            continue;
        };
        let target = root.join(&rel);
        let already_there = match std::fs::metadata(&target) {
            Ok(meta) if meta.len() == entry.size => {
                launchcraft_sync::diff::hash_local_file(&target)
                    .map(|hash| hash == entry.hash)
                    .unwrap_or(false)
            }
            _ => false,
        };
        if already_there {
            plan.unchanged_files += 1;
            plan.actions.push(SyncAction::Unchanged { rel_path: rel });
            continue;
        }
        plan.download_files += 1;
        plan.download_bytes += entry.size;
        plan.actions.push(SyncAction::Download {
            rel_path: rel,
            hash: entry.hash.clone(),
            size: entry.size,
            zstd_compressed: entry.compressed_size.is_some(),
        });
    }
    plan
}

/// Fetch, plan and apply one runtime, then record it. Returns the install
/// version (the id PLAY needs) so callers can confirm what landed.
pub async fn install_runtime(
    paths: &LauncherPaths,
    server_url: &str,
    id: &str,
    tx: &Sender<JobEvent>,
    ctx: &egui::Context,
) -> Result<String, String> {
    safe_runtime_id(id)?;
    let repo = runtime_repo_url(server_url, id);

    report_progress(tx, ctx, format!("Fetching runtime {id}…"), Some(0.0), false);
    let manifest: PackManifest = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| e.to_string())?
        .get(format!("{repo}/manifest.json"))
        .send()
        .await
        .map_err(|e| format!("could not reach {repo}/manifest.json: {e}"))?
        .error_for_status()
        .map_err(|e| format!("server error: {e}"))?
        .json()
        .await
        .map_err(|e| format!("bad runtime manifest: {e}"))?;

    if manifest.pack_version.trim().is_empty() {
        return Err(format!("runtime {id} manifest has no install version"));
    }
    // The manifest's own id must match what we asked for: a server that
    // answers `1.20.1--fabric--x` with someone else's store would otherwise
    // write foreign files into the shared root.
    if manifest.pack_id != id {
        return Err(format!(
            "runtime manifest is for '{}' but {id} was requested",
            manifest.pack_id
        ));
    }

    let plan = plan_install(&paths.root, &manifest);
    if !plan.refused.is_empty() {
        report(
            tx,
            ctx,
            format!(
                "Runtime {id}: refused {} path(s) outside versions/, assets/, libraries/.",
                plan.refused.len()
            ),
            false,
        );
    }
    if plan.refused.len() == manifest.files.len() && !manifest.files.is_empty() {
        return Err(format!("runtime {id} contained no installable files"));
    }

    report_progress(
        tx,
        ctx,
        format!(
            "Runtime {id}: {} new / {} already installed",
            plan.download_files, plan.unchanged_files
        ),
        Some(0.0),
        false,
    );

    if plan.download_files > 0 {
        paths.ensure_dirs().map_err(|e| e.to_string())?;
        let downloader = CasDownloader::new(CasStore::new(&paths.cas_cache), DOWNLOAD_CONCURRENCY);
        let (tx2, ctx2) = (tx.clone(), ctx.clone());
        let last_sent = std::sync::Mutex::new(-1.0f32);
        let total = plan.download_bytes.max(1);
        // The progress callback must be 'static, so it owns what it reports.
        let id_label = id.to_string();
        downloader
            .sync_instance(&repo, &plan.actions, &paths.root, move |p| {
                let frac = (p.bytes_completed as f64 / total as f64).min(1.0) as f32;
                let done = p.total_files > 0 && p.files_completed == p.total_files;
                let mut last = last_sent.lock().unwrap_or_else(|e| e.into_inner());
                if done || frac >= *last + 0.005 {
                    *last = frac;
                    report_progress(
                        &tx2,
                        &ctx2,
                        format!(
                            "⬇ {:.0}% runtime {} — {}/{} files",
                            frac * 100.0,
                            id_label,
                            p.files_completed,
                            p.total_files
                        ),
                        Some(frac),
                        false,
                    );
                }
            })
            .await
            .map_err(|e| format!("runtime download failed: {e}"))?;
    }

    // Install-time check: the profile and client jar must be on disk, or PLAY
    // would fail later with a confusing message.
    let install_version = manifest.pack_version.clone();
    let dir = paths.versions.join(&install_version);
    if !dir.join(format!("{install_version}.json")).is_file() {
        return Err(format!(
            "runtime {id} did not provide versions/{install_version}/{install_version}.json"
        ));
    }

    let entry = InstalledRuntime {
        id: id.to_string(),
        install_version: install_version.clone(),
        server_url: base_url(server_url),
        file_count: manifest.files.len() as u64,
        total_bytes: manifest.files.values().map(|f| f.size).sum(),
        manifest_hash: blake3::hash(&serde_json::to_vec(&manifest).map_err(|e| e.to_string())?)
            .to_hex()
            .to_string(),
        installed_at: chrono::Utc::now().to_rfc3339(),
    };
    let mut registry = RuntimeRegistry::load(&paths.config);
    // Say what this sync actually changed, read before the record is replaced:
    // first install, an identical repeat, or a runtime the server rebuilt.
    let summary = match registry.find(id) {
        None => "first install".to_string(),
        Some(before) if before.manifest_hash == entry.manifest_hash => {
            "unchanged since last sync".to_string()
        }
        Some(before) => format!(
            "server rebuilt it: {} → {} files",
            before.file_count, entry.file_count
        ),
    };
    registry.record(entry.clone());
    registry
        .save(&paths.config)
        .map_err(|e| format!("could not write runtimes.json: {e}"))?;

    report_progress(
        tx,
        ctx,
        format!("Runtime {id} ready as {install_version} ({summary})."),
        Some(1.0),
        true,
    );
    Ok(install_version)
}

/// Install the runtime a pack needs, if the server names one. Failures are
/// reported but non-fatal: the pack is still usable once the user installs the
/// version from the Versions tab, which stays the manual fallback.
pub async fn ensure_for_pack(
    paths: &LauncherPaths,
    server_url: &str,
    runtime_id: Option<&str>,
    tx: &Sender<JobEvent>,
    ctx: &egui::Context,
) -> Option<String> {
    let id = runtime_id.filter(|i| !i.is_empty())?;
    // Always re-plan: files that verify against the manifest are skipped, so
    // this both picks up a runtime the server rebuilt and heals a truncated
    // jar — at the cost of hashing what is already on disk, which blake3 does
    // at about a GB per second.
    match install_runtime(paths, server_url, id, tx, ctx).await {
        Ok(version) => Some(version),
        Err(e) => {
            report(
                tx,
                ctx,
                format!("Runtime {id} not installed: {e} — install it on the Versions tab."),
                true,
            );
            None
        }
    }
}

/// Background wrapper used by the Home tab's install flow.
pub fn spawn_ensure(
    rt: &tokio::runtime::Runtime,
    ctx: egui::Context,
    tx: Sender<JobEvent>,
    paths: LauncherPaths,
    server_url: String,
    runtime_id: String,
) {
    rt.spawn(async move {
        let _ = ensure_for_pack(&paths, &server_url, Some(&runtime_id), &tx, &ctx).await;
        ctx.request_repaint();
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use launchcraft_core::manifest::ManifestEntry;
    use std::collections::BTreeMap;

    fn entry(hash: String, size: u64) -> ManifestEntry {
        ManifestEntry {
            hash,
            size,
            compressed_size: Some(size / 2 + 1),
            user_override: false,
            executable: false,
        }
    }

    fn manifest(files: Vec<(&str, &[u8])>) -> PackManifest {
        let mut map = BTreeMap::new();
        for (path, bytes) in files {
            map.insert(
                path.to_string(),
                entry(blake3::hash(bytes).to_hex().to_string(), bytes.len() as u64),
            );
        }
        PackManifest {
            pack_id: "1.20.1--fabric--0.15.11".into(),
            pack_version: "1.20.1-fabric-0.15.11".into(),
            game_version: "1.20.1".into(),
            loader: Some("fabric-0.15.11".into()),
            created_at: chrono::Utc::now(),
            files: map,
            user_overrides: vec![],
        }
    }

    #[test]
    fn only_the_three_runtime_trees_are_writable() {
        assert!(safe_runtime_path("versions/1.20.1/1.20.1.jar").is_some());
        assert!(safe_runtime_path("assets/indexes/5.json").is_some());
        assert!(safe_runtime_path("libraries/com/x/y/1/y-1.jar").is_some());
        assert!(safe_runtime_path("libraries\\com\\x\\y\\1\\y-1.jar").is_some());

        // Everything else is refused, including escapes and dotfiles.
        for blocked in [
            "config.json",
            "instances/alice/instance.json",
            "cas/objects/aa/aabb",
            "../../etc/passwd",
            "versions/../../escape",
            "/versions/abs.json",
            "versions/",
            "versions//x.json",
            "",
        ] {
            assert_eq!(safe_runtime_path(blocked), None, "accepted {blocked:?}");
        }
    }

    #[test]
    fn plans_skip_verified_files_and_refetch_the_rest() {
        let root = tempfile::tempdir().unwrap();
        let jar = b"vanilla-client-jar".to_vec();
        let lib = b"library-bytes".to_vec();
        let m = manifest(vec![
            ("versions/1.20.1/1.20.1.jar", &jar),
            ("libraries/com/x/y/1/y-1.jar", &lib),
        ]);

        // Nothing on disk yet: both are downloads.
        let plan = plan_install(root.path(), &m);
        assert_eq!(plan.download_files, 2);
        assert_eq!(plan.unchanged_files, 0);
        assert_eq!(plan.download_bytes, (jar.len() + lib.len()) as u64);

        // Place one file exactly as advertised, and corrupt the other.
        std::fs::create_dir_all(root.path().join("versions/1.20.1")).unwrap();
        std::fs::write(root.path().join("versions/1.20.1/1.20.1.jar"), &jar).unwrap();
        std::fs::create_dir_all(root.path().join("libraries/com/x/y/1")).unwrap();
        std::fs::write(root.path().join("libraries/com/x/y/1/y-1.jar"), b"trunc").unwrap();

        let plan = plan_install(root.path(), &m);
        assert_eq!(plan.unchanged_files, 1, "good jar must be left alone");
        assert_eq!(plan.download_files, 1, "short library must be refetched");
        assert!(plan
            .actions
            .iter()
            .any(|a| matches!(a, SyncAction::Unchanged { rel_path } if rel_path == "versions/1.20.1/1.20.1.jar")));
        assert!(plan
            .actions
            .iter()
            .any(|a| matches!(a, SyncAction::Download { rel_path, .. } if rel_path == "libraries/com/x/y/1/y-1.jar")));
        assert!(plan.refused.is_empty());
    }

    #[test]
    fn hostile_manifest_paths_are_refused_not_written() {
        let root = tempfile::tempdir().unwrap();
        let m = manifest(vec![
            ("../../config.json", b"evil"),
            ("instances/victim/instance.json", b"evil"),
            ("versions/1.20.1/1.20.1.json", b"legit"),
        ]);
        let plan = plan_install(root.path(), &m);
        assert_eq!(plan.refused.len(), 2);
        assert_eq!(plan.download_files, 1);
        assert!(plan.actions.iter().all(|a| match a {
            SyncAction::Download { rel_path, .. } => rel_path.starts_with("versions/"),
            _ => true,
        }));
    }

    #[test]
    fn registry_roundtrip_keeps_one_record_per_runtime() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config");
        std::fs::create_dir_all(&config).unwrap();

        let mut registry = RuntimeRegistry::default();
        registry.record(InstalledRuntime {
            id: "1.20.1--fabric--0.15.11".into(),
            install_version: "1.20.1-fabric-0.15.11".into(),
            server_url: "http://h:9090".into(),
            file_count: 63,
            total_bytes: 84_000_000,
            manifest_hash: "aa".into(),
            installed_at: "2026-01-01T00:00:00Z".into(),
        });
        assert!(registry.find("1.20.1--fabric--0.15.11").is_some());
        assert!(registry.find("1.7.10--vanilla").is_none());

        // Re-recording the same runtime replaces rather than duplicates.
        registry.record(InstalledRuntime {
            file_count: 64,
            manifest_hash: "bb".into(),
            ..registry.find("1.20.1--fabric--0.15.11").unwrap().clone()
        });
        assert_eq!(registry.runtimes.len(), 1);
        assert_eq!(
            registry.find("1.20.1--fabric--0.15.11").unwrap().file_count,
            64
        );

        registry.save(&config).unwrap();
        assert_eq!(RuntimeRegistry::load(&config), registry);
        // A missing file reads as empty, never as an error.
        assert_eq!(
            RuntimeRegistry::load(&dir.path().join("nope")),
            RuntimeRegistry::default()
        );
    }

    #[test]
    fn ids_and_urls_are_fenced() {
        assert!(safe_runtime_id("1.20.1--fabric--0.15.11").is_ok());
        assert!(safe_runtime_id("../etc").is_err());
        assert!(safe_runtime_id("").is_err());
        assert!(safe_runtime_id("has space").is_err());
        assert_eq!(
            runtime_repo_url("http://h:9090/", "1.20.1--vanilla"),
            "http://h:9090/runtimes/1.20.1--vanilla"
        );
    }

    /// An in-process runtime repository: serves `/runtimes/<id>/manifest.json`
    /// and `<id>/cas/objects/xx/<hash>` and nothing else. One connection per
    /// request (`connection: close`), which is what reqwest does when the
    /// server says so, keeps this loop trivially single-threaded.
    fn serve_runtime(manifest: String, blobs: BTreeMap<String, Vec<u8>>) -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                use std::io::{Read, Write};
                let mut sock = stream;
                let mut buf = [0u8; 4096];
                let Ok(n) = sock.read(&mut buf) else { continue };
                let head = String::from_utf8_lossy(&buf[..n]).into_owned();
                let Some(raw) = head.split_whitespace().nth(1) else {
                    continue;
                };
                let (status, body) = if raw.ends_with("/manifest.json") {
                    ("200 OK", manifest.as_bytes().to_vec())
                } else {
                    // The blob route ends in the hash; look it up by suffix.
                    let hash = raw.rsplit('/').next().unwrap_or_default().to_string();
                    match blobs.get(&hash) {
                        Some(bytes) => ("200 OK", bytes.clone()),
                        None => ("404 Not Found", Vec::new()),
                    }
                };
                let reply = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: application/octet-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                let _ = sock.write_all(reply.as_bytes());
                let _ = sock.write_all(&body);
                let _ = sock.flush();
            }
        });
        port
    }

    /// A runtime holding exactly one file: the manifest names the profile json
    /// and the client jar, each stored as a zstd blob (like the server builds).
    fn one_file_runtime(content: &[u8]) -> (PackManifest, BTreeMap<String, Vec<u8>>) {
        let hash = blake3::hash(content).to_hex().to_string();
        let compressed = zstd::encode_all(content, 3).unwrap();
        let mut blobs = BTreeMap::new();
        blobs.insert(hash.clone(), compressed);
        let mut files = BTreeMap::new();
        files.insert(
            "versions/1.20.1-fabric-0.15.11/1.20.1-fabric-0.15.11.json".to_string(),
            ManifestEntry {
                hash: hash.clone(),
                size: content.len() as u64,
                compressed_size: Some(content.len() as u64),
                user_override: false,
                executable: false,
            },
        );
        // The jar is the same bytes: shared blobs must not be fetched twice.
        files.insert(
            "versions/1.20.1-fabric-0.15.11/1.20.1-fabric-0.15.11.jar".to_string(),
            ManifestEntry {
                hash,
                size: content.len() as u64,
                compressed_size: Some(content.len() as u64),
                user_override: false,
                executable: false,
            },
        );
        let manifest = PackManifest {
            pack_id: "1.20.1--fabric--0.15.11".into(),
            pack_version: "1.20.1-fabric-0.15.11".into(),
            game_version: "1.20.1".into(),
            loader: Some("fabric-0.15.11".into()),
            created_at: chrono::Utc::now(),
            files,
            user_overrides: vec![],
        };
        (manifest, blobs)
    }

    fn temp_paths(dir: &Path) -> LauncherPaths {
        LauncherPaths {
            root: dir.to_path_buf(),
            instances: dir.join("instances"),
            cas_cache: dir.join("cas"),
            runtimes: dir.join("jres"),
            versions: dir.join("versions"),
            assets: dir.join("assets"),
            libraries: dir.join("libraries"),
            config: dir.join("config"),
            is_portable: true,
        }
    }

    #[test]
    fn installs_a_runtime_from_the_server_and_skips_the_second_pass() {
        let dir = tempfile::tempdir().unwrap();
        let paths = temp_paths(dir.path());
        let (manifest, blobs) = one_file_runtime(b"{\"mainClass\":\"net.fabricmc.KnotClient\"}");
        let port = serve_runtime(serde_json::to_string(&manifest).unwrap(), blobs);
        let server = format!("http://127.0.0.1:{port}");

        let rt = tokio::runtime::Runtime::new().unwrap();
        let (tx, rx) = std::sync::mpsc::channel::<JobEvent>();
        let ctx = egui::Context::default();

        let version = rt
            .block_on(install_runtime(
                &paths,
                &server,
                "1.20.1--fabric--0.15.11",
                &tx,
                &ctx,
            ))
            .expect("install");
        assert_eq!(version, "1.20.1-fabric-0.15.11");

        // Both manifest paths landed in the shared data root.
        let profile = paths
            .versions
            .join("1.20.1-fabric-0.15.11")
            .join("1.20.1-fabric-0.15.11.json");
        let jar = paths
            .versions
            .join("1.20.1-fabric-0.15.11")
            .join("1.20.1-fabric-0.15.11.jar");
        assert!(profile.is_file(), "profile missing at {profile:?}");
        assert!(jar.is_file(), "jar missing at {jar:?}");
        assert_eq!(
            std::fs::read_to_string(&profile).unwrap(),
            "{\"mainClass\":\"net.fabricmc.KnotClient\"}"
        );

        // Recorded, so the caller can skip the next install.
        let registry = RuntimeRegistry::load(&paths.config);
        let recorded = registry.find("1.20.1--fabric--0.15.11").unwrap();
        assert_eq!(recorded.install_version, "1.20.1-fabric-0.15.11");
        assert_eq!(recorded.file_count, 2);
        assert!(!recorded.manifest_hash.is_empty());
        assert_eq!(recorded.server_url, server);

        // Second pass: everything is already verified on disk, so the plan
        // downloads nothing and the log says the runtime is unchanged.
        let plan = plan_install(&paths.root, &manifest);
        assert_eq!(plan.download_files, 0);
        assert_eq!(plan.unchanged_files, 2);
        rt.block_on(install_runtime(
            &paths,
            &server,
            "1.20.1--fabric--0.15.11",
            &tx,
            &ctx,
        ))
        .unwrap();
        let mut lines = Vec::new();
        while let Ok(event) = rx.try_recv() {
            lines.push(event.line);
        }
        assert!(
            lines
                .iter()
                .any(|l| l.contains("unchanged since last sync")),
            "expected an unchanged report, saw {lines:?}"
        );
    }

    /// The real contract, end to end: a launchcraft-server stages Minecraft
    /// 1.7.10 (the Java 8 era, smallest download) from Mojang into its shared
    /// runtime CAS, and this launcher installs it into a data root over HTTP.
    /// Skipped by default because it needs the network.
    #[tokio::test]
    #[ignore = "fetches a real Minecraft runtime from Mojang; run with --ignored"]
    async fn installs_a_runtime_staged_by_a_real_server() {
        let server_dir = tempfile::tempdir().unwrap();
        let client_dir = tempfile::tempdir().unwrap();

        let cfg = launchcraft_server::ServerConfig {
            addr: "127.0.0.1:0".into(),
            data_dir: server_dir.path().to_path_buf(),
            max_upload_mb: 8,
            ..Default::default()
        };
        let state = launchcraft_server::AppState::new(&cfg).unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{port}"))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, launchcraft_server::build_router(state))
                .await
                .unwrap();
        });
        let server = format!("http://{addr}");

        // Ask the server to stage the runtime, then wait for it to be ready.
        reqwest::Client::new()
            .post(format!("{server}/console/runtimes"))
            .json(&serde_json::json!({ "game_version": "1.7.10" }))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        let mut ready = None;
        for _ in 0..120 {
            let list = reqwest::Client::new()
                .get(format!("{server}/runtimes"))
                .send()
                .await
                .unwrap()
                .json::<serde_json::Value>()
                .await
                .unwrap();
            let row = list["runtimes"]
                .as_array()
                .unwrap()
                .iter()
                .find(|r| r["id"] == serde_json::json!("1.7.10--vanilla"))
                .cloned();
            if row.is_some() {
                ready = row;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
        let row = ready.expect("server never published the 1.7.10 runtime");
        assert_eq!(row["install_version"], serde_json::json!("1.7.10"));
        assert!(row["file_count"].as_u64().unwrap() > 20);

        let paths = temp_paths(client_dir.path());
        let (tx, _rx) = std::sync::mpsc::channel::<JobEvent>();
        let ctx = egui::Context::default();

        // A file the server does not know about must survive: a runtime sync
        // is additive, it never prunes a shared directory.
        std::fs::create_dir_all(paths.libraries.join("com/keepme")).unwrap();
        std::fs::write(paths.libraries.join("com/keepme/keep.txt"), b"mine").unwrap();

        let version = install_runtime(&paths, &server, "1.7.10--vanilla", &tx, &ctx)
            .await
            .expect("install from a real server");
        assert_eq!(version, "1.7.10");

        let profile = paths.versions.join("1.7.10").join("1.7.10.json");
        let jar = paths.versions.join("1.7.10").join("1.7.10.jar");
        assert!(profile.is_file());
        assert!(jar.is_file());
        assert_eq!(
            std::fs::metadata(&jar).unwrap().len(),
            5_256_245,
            "1.7.10 client jar"
        );
        assert!(std::fs::read_to_string(&profile)
            .unwrap()
            .contains("\"mainClass\""));
        assert_eq!(
            std::fs::read_to_string(paths.libraries.join("com/keepme/keep.txt")).unwrap(),
            "mine"
        );
        assert!(RuntimeRegistry::load(&paths.config)
            .find("1.7.10--vanilla")
            .is_some());

        // Re-installing changes nothing on the wire: every file verifies locally.
        let manifest: PackManifest = reqwest::Client::new()
            .get(format!("{server}/runtimes/1.7.10--vanilla/manifest.json"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let plan = plan_install(&paths.root, &manifest);
        assert_eq!(plan.download_files, 0, "second pass must not re-download");
        assert!(plan.unchanged_files > 20);
    }

    #[test]
    fn a_manifest_for_another_runtime_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let paths = temp_paths(dir.path());
        let (mut manifest, blobs) = one_file_runtime(b"profile");
        manifest.pack_id = "1.16.5--vanilla".into();
        let port = serve_runtime(serde_json::to_string(&manifest).unwrap(), blobs);

        let rt = tokio::runtime::Runtime::new().unwrap();
        let (tx, _rx) = std::sync::mpsc::channel::<JobEvent>();
        let ctx = egui::Context::default();
        let err = rt
            .block_on(install_runtime(
                &paths,
                &format!("http://127.0.0.1:{port}"),
                "1.20.1--fabric--0.15.11",
                &tx,
                &ctx,
            ))
            .expect_err("cross-delivery must fail");
        assert!(err.contains("was requested"), "unexpected error: {err}");
        assert!(!paths.versions.join("1.16.5--vanilla").exists());
    }
}
