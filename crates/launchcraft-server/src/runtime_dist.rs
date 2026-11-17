//! Server-side Minecraft runtime distribution (Phase 11).
//!
//! A *runtime* is everything a launcher needs to start one Minecraft version
//! with one loader: the `versions/` profiles and client jar, the `assets/`
//! index (objects are opt-in), and the applicable `libraries/` jars. The
//! server fetches them once into a staging tree with
//! [`launchcraft_runtime::meta::MinecraftClient`], then packs that tree into a
//! **shared** content-addressed store plus a `manifest.json` in the same shape
//! the pack builder emits — so a client can sync a runtime with the normal
//! `SyncEngine` instead of talking to Mojang.
//!
//! Storage lives beside the pack workspaces and never inside one:
//!
//! ```text
//! <data>/runtimes/registry.json          # runtime records + build status
//! <data>/runtimes/cas/objects/xx/<hash>  # zstd blobs, shared by every runtime
//! <data>/runtimes/<id>/manifest.json     # one runtime's file list
//! <data>/runtimes/staging/<id>/          # temporary fetch tree, removed after packing
//! ```
//!
//! Because the CAS is shared, two runtimes that differ only in loader reuse the
//! same client jar and library blobs, and **deleting a runtime never deletes a
//! blob another runtime still references** — that is what
//! [`console_gc`] (unreferenced runtimes, then orphaned blobs) is for. Pack
//! deletion is unaffected either: pack distributions keep their own CAS.
//!
//! Runtime ids are `{mc}--vanilla` or `{mc}--{loader}--{loader_version}`.
//! Forge and NeoForge are rejected: their profiles come from an installer jar,
//! which needs a JDK in the server image (`Dockerfile.server`), so packs using
//! them keep shipping the installer through `src/loader/` as before.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use walkdir::WalkDir;

use launchcraft_core::manifest::{ManifestEntry, PackManifest};
use launchcraft_core::paths::LauncherPaths;
use launchcraft_runtime::meta::{LoaderKind, MinecraftClient};

use crate::loader::LoaderSpec;
use crate::packs::PackRegistry;
use crate::store::write_atomic;
use crate::{ApiError, AppState};

/// zstd level for runtime blobs, matching `BuildConfig`'s default.
const COMPRESSION_LEVEL: i32 = 3;

/// The three staging sub-trees that become manifest entries. Everything else
/// `LauncherPaths` creates (`instances/`, `cas/`, …) stays empty and out.
const RUNTIME_DIRS: [&str; 3] = ["versions", "assets", "libraries"];

// ---------------------------------------------------------------------- keys

/// Which runtime a pack (or an operator) is asking for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeKey {
    pub mc_version: String,
    /// `"fabric"` / `"quilt"`, or `None` for plain vanilla.
    pub loader: Option<String>,
    pub loader_version: Option<String>,
}

impl RuntimeKey {
    pub fn vanilla(mc_version: impl Into<String>) -> Self {
        Self {
            mc_version: mc_version.into(),
            loader: None,
            loader_version: None,
        }
    }

    /// Stable store id, also the directory name under `<data>/runtimes/`.
    pub fn id(&self) -> String {
        match (&self.loader, &self.loader_version) {
            (Some(kind), Some(version)) => format!("{}--{}--{}", self.mc_version, kind, version),
            _ => format!("{}--vanilla", self.mc_version),
        }
    }

    /// Mojang-style version id the runtime installs (`1.20.1`,
    /// `1.20.1-fabric-0.15.11`) — recorded as the manifest's `pack_version`.
    pub fn install_version(&self) -> String {
        match (&self.loader, &self.loader_version) {
            (Some(kind), Some(version)) => format!("{}-{}-{}", self.mc_version, kind, version),
            _ => self.mc_version.clone(),
        }
    }

    /// Loader label as packs and `BuildConfig` spell it (`fabric-0.15.11`).
    pub fn loader_label(&self) -> Option<String> {
        match (&self.loader, &self.loader_version) {
            (Some(kind), Some(version)) => Some(format!("{kind}-{version}")),
            _ => None,
        }
    }

    /// `true` when every component is a plain filesystem/URL token.
    fn is_safe(&self) -> bool {
        let ok = |s: &str| {
            !s.is_empty()
                && s.len() <= 64
                && !s.starts_with('.')
                && s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '+'))
        };
        ok(&self.mc_version)
            && self.loader.as_deref().is_none_or(ok)
            && self.loader_version.as_deref().is_none_or(ok)
    }

    /// Reject loaders whose profile only an installer jar can produce.
    fn is_buildable(&self) -> bool {
        self.loader
            .as_deref()
            .is_none_or(|k| matches!(k, "fabric" | "quilt"))
    }
}

/// Runtime for a pack import's `{loader, game_version}` pair. Empty loaders
/// mean vanilla; a loader the server cannot parse (or a missing game version)
/// means no runtime at all. Forge/NeoForge map to a real key that
/// [`RuntimeKey::is_buildable`] rejects, so callers can tell "unsupported"
/// from "nothing to do".
pub fn key_for_plan(loader: &str, game_version: &str) -> Option<RuntimeKey> {
    let game = game_version.trim();
    if game.is_empty() {
        return None;
    }
    let raw = loader.trim();
    if raw.is_empty() {
        return Some(RuntimeKey::vanilla(game));
    }
    let spec = LoaderSpec::detect(raw, game)?;
    Some(RuntimeKey {
        mc_version: game.to_string(),
        loader: Some(spec.kind),
        loader_version: Some(spec.version),
    })
}

// ------------------------------------------------------------------ registry

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuntimeStatus {
    Building,
    Ready,
    Error,
}

/// One row of `registry.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeEntry {
    pub id: String,
    pub mc_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loader: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loader_version: Option<String>,
    pub install_version: String,
    pub status: RuntimeStatus,
    /// Whether asset objects were fetched (index json is always stored).
    #[serde(default)]
    pub assets: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default)]
    pub file_count: u64,
    /// Uncompressed total, for display.
    #[serde(default)]
    pub total_bytes: u64,
    /// Bytes this runtime's manifest references in the shared CAS. New
    /// blobs only — the shared store is accounted globally by [`sweep`].
    #[serde(default)]
    pub stored_bytes: u64,
    /// Java major version this runtime needs (`jres` keys one JDK per major,
    /// so a 1.7.10 and a 1.20.5 runtime on one server coexist).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required_java: Option<u32>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct RuntimeRegistry {
    #[serde(default)]
    pub runtimes: Vec<RuntimeEntry>,
}

impl RuntimeRegistry {
    pub fn find(&self, id: &str) -> Option<&RuntimeEntry> {
        self.runtimes.iter().find(|r| r.id == id)
    }

    pub fn find_mut(&mut self, id: &str) -> Option<&mut RuntimeEntry> {
        self.runtimes.iter_mut().find(|r| r.id == id)
    }

    /// Insert or replace, keeping the original `created_at` on replace.
    pub fn upsert(&mut self, mut entry: RuntimeEntry) {
        if let Some(idx) = self.runtimes.iter().position(|r| r.id == entry.id) {
            entry.created_at = self.runtimes[idx].created_at.clone();
            self.runtimes[idx] = entry;
        } else {
            self.runtimes.push(entry);
        }
        self.runtimes.sort_by(|a, b| a.id.cmp(&b.id));
    }

    pub fn remove(&mut self, id: &str) -> Option<RuntimeEntry> {
        let idx = self.runtimes.iter().position(|r| r.id == id)?;
        Some(self.runtimes.remove(idx))
    }

    /// Pack ids that point at a runtime.
    pub fn referenced_by(packs: &PackRegistry, id: &str) -> Vec<String> {
        packs
            .packs
            .iter()
            .filter(|p| p.runtime_id.as_deref() == Some(id))
            .map(|p| p.id.clone())
            .collect()
    }
}

/// Filesystem side of runtime storage.
#[derive(Debug, Clone)]
pub struct RuntimeStore {
    root: PathBuf,
}

impl RuntimeStore {
    pub fn new(data_dir: &Path) -> Self {
        Self {
            root: data_dir.join("runtimes"),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn registry_path(&self) -> PathBuf {
        self.root.join("registry.json")
    }

    pub fn cas_dir(&self) -> PathBuf {
        self.root.join("cas").join("objects")
    }

    pub fn manifest_path(&self, id: &str) -> PathBuf {
        self.root.join(id).join("manifest.json")
    }

    pub fn staging_dir(&self, id: &str) -> PathBuf {
        self.root.join("staging").join(id)
    }

    /// Shared blob path for one BLAKE3 hash.
    pub fn blob_path(&self, hash: &str) -> PathBuf {
        self.cas_dir().join(&hash[..2]).join(hash)
    }

    pub fn ensure_layout(&self) -> std::io::Result<()> {
        fs::create_dir_all(self.cas_dir())?;
        Ok(())
    }

    pub fn load_registry(&self) -> Result<RuntimeRegistry, ApiError> {
        let path = self.registry_path();
        if !path.is_file() {
            return Ok(RuntimeRegistry::default());
        }
        let text = fs::read_to_string(&path).map_err(|e| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("could not read {}: {e}", path.display()),
            )
        })?;
        serde_json::from_str::<RuntimeRegistry>(&text).map_err(|e| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("could not parse {}: {e}", path.display()),
            )
        })
    }

    pub fn save_registry(&self, registry: &RuntimeRegistry) -> Result<(), ApiError> {
        fs::create_dir_all(&self.root)?;
        let body = serde_json::to_vec_pretty(registry)?;
        write_atomic(&self.registry_path(), &body)?;
        Ok(())
    }

    pub fn read_manifest(&self, id: &str) -> Option<PackManifest> {
        let text = fs::read_to_string(self.manifest_path(id)).ok()?;
        serde_json::from_str::<PackManifest>(&text).ok()
    }
}

// ------------------------------------------------------------------ building

/// Launcher paths for one staging tree. All fields are public on purpose in
/// `launchcraft-core`, so the server can point the runtime fetcher at a
/// throwaway directory instead of the operator's own launcher data.
fn staging_paths(root: &Path) -> LauncherPaths {
    LauncherPaths {
        instances: root.join("instances"),
        cas_cache: root.join("cas"),
        runtimes: root.join("jres"),
        versions: root.join("versions"),
        assets: root.join("assets"),
        libraries: root.join("libraries"),
        config: root.join("config"),
        root: root.to_path_buf(),
        is_portable: true,
    }
}

/// Start (or join) a runtime build. Returns immediately when the runtime is
/// already queued or built; the fetch itself runs in a background task because
/// a full Fabric runtime is hundreds of jars.
pub async fn schedule(
    state: &Arc<AppState>,
    key: &RuntimeKey,
    assets: Option<bool>,
    force: bool,
) -> Result<Value, ApiError> {
    if !key.is_safe() {
        return Err(ApiError::bad_request(format!(
            "unsafe runtime request '{}'",
            key.id()
        )));
    }
    if !key.is_buildable() {
        let loader = key.loader_label().unwrap_or_default();
        return Err(ApiError::bad_request(format!(
            "server-side runtime builds support vanilla, Fabric and Quilt — '{loader}' still \
             needs its installer jar unpacked into a launch profile. Java itself is available \
             (see /console/jres); the installer-format reader is the missing piece. The pack \
             still gets the installer through src/loader/."
        )));
    }

    let id = key.id();
    let want_assets = assets.unwrap_or(state.runtime_assets);
    let _guard = state.runtime_lock.lock().await;
    let mut registry = state.runtimes.load_registry()?;
    if let Some(existing) = registry.find(&id) {
        match existing.status {
            RuntimeStatus::Building => {
                return Ok(json!({ "id": id, "status": "building", "queued": false }))
            }
            RuntimeStatus::Ready if !force && existing.assets == want_assets => {
                return Ok(entry_json(existing, Vec::new()))
            }
            _ => {}
        }
    }

    let now = chrono::Utc::now().to_rfc3339();
    let entry = RuntimeEntry {
        id: id.clone(),
        mc_version: key.mc_version.clone(),
        loader: key.loader.clone(),
        loader_version: key.loader_version.clone(),
        install_version: key.install_version(),
        status: RuntimeStatus::Building,
        assets: want_assets,
        error: None,
        created_at: now.clone(),
        updated_at: now,
        file_count: 0,
        total_bytes: 0,
        stored_bytes: 0,
        // Known from the version alone; refined from the fetched profile so
        // packs whose profile declares a different Java show the truth.
        required_java: Some(crate::jres::required_major(&key.mc_version, None)),
    };
    registry.upsert(entry.clone());
    state.runtimes.save_registry(&registry)?;
    drop(_guard);

    let st = state.clone();
    let key = key.clone();
    tokio::spawn(async move {
        run_build(st, key, want_assets).await;
    });

    tracing::info!(
        "queued runtime '{id}' (assets: {})",
        if want_assets { "full" } else { "index only" }
    );
    Ok(json!({ "id": id, "status": "building", "queued": true }))
}

async fn run_build(state: Arc<AppState>, key: RuntimeKey, assets: bool) {
    let id = key.id();
    let outcome = build_runtime(&state, &key, assets).await;
    let _ = fs::remove_dir_all(state.runtimes.staging_dir(&id));

    let _guard = state.runtime_lock.lock().await;
    let mut registry = match state.runtimes.load_registry() {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("runtime '{id}': cannot read registry: {e}");
            return;
        }
    };
    let existing = registry.find(&id).cloned();
    let now = chrono::Utc::now().to_rfc3339();
    let mut entry = existing.unwrap_or_else(|| RuntimeEntry {
        id: id.clone(),
        mc_version: key.mc_version.clone(),
        loader: key.loader.clone(),
        loader_version: key.loader_version.clone(),
        install_version: key.install_version(),
        status: RuntimeStatus::Building,
        assets,
        error: None,
        created_at: now.clone(),
        updated_at: now.clone(),
        file_count: 0,
        total_bytes: 0,
        stored_bytes: 0,
        required_java: Some(crate::jres::required_major(&key.mc_version, None)),
    });
    entry.updated_at = now;
    entry.assets = assets;
    match outcome {
        Ok((manifest, profile_java)) => {
            entry.status = RuntimeStatus::Ready;
            entry.error = None;
            entry.file_count = manifest.files.len() as u64;
            entry.total_bytes = manifest.files.values().map(|f| f.size).sum();
            entry.stored_bytes = manifest
                .files
                .values()
                .filter_map(|f| f.compressed_size)
                .sum();
            entry.required_java = Some(crate::jres::required_major(
                &key.mc_version,
                profile_java.or(entry.required_java),
            ));
            tracing::info!(
                "runtime '{id}' ready: {} file(s), {} uncompressed bytes, Java {}",
                entry.file_count,
                entry.total_bytes,
                entry.required_java.unwrap_or(0),
            );
        }
        Err(message) => {
            entry.status = RuntimeStatus::Error;
            entry.error = Some(message.clone());
            tracing::warn!("runtime '{id}' build failed: {message}");
        }
    }
    registry.upsert(entry);
    if let Err(e) = state.runtimes.save_registry(&registry) {
        tracing::error!("runtime '{id}': cannot save registry: {e}");
    }
}

/// Fetch into staging, then pack the tree into the shared CAS + manifest.
/// Returns the manifest plus the Java major the fetched profile declares
/// (`javaVersion.majorVersion`), which is authoritative over the version-name
/// table in [`crate::jres::required_major`].
async fn build_runtime(
    state: &Arc<AppState>,
    key: &RuntimeKey,
    assets: bool,
) -> Result<(PackManifest, Option<u32>), String> {
    let id = key.id();
    let staging = state.runtimes.staging_dir(&id);
    if staging.exists() {
        fs::remove_dir_all(&staging).map_err(|e| format!("cannot reset {staging:?}: {e}"))?;
    }
    fs::create_dir_all(&staging).map_err(|e| format!("cannot create {staging:?}: {e}"))?;

    let client = MinecraftClient::with_urls(
        staging_paths(&staging),
        state.meta_base.clone(),
        state.fabric_base.clone(),
        state.quilt_base.clone(),
        state.resources_base.clone(),
    );
    let paths = staging_paths(&staging);
    paths
        .ensure_dirs()
        .map_err(|e| format!("cannot create staging layout: {e}"))?;
    drop(paths);

    let list = client
        .version_list()
        .await
        .map_err(|e| format!("version manifest: {e}"))?;
    let entry = list.find(&key.mc_version).ok_or_else(|| {
        format!(
            "'{}' is not in the Minecraft version manifest",
            key.mc_version
        )
    })?;

    let report = match (&key.loader, &key.loader_version) {
        (Some(kind), Some(version)) => {
            let loader = if kind == "quilt" {
                LoaderKind::Quilt
            } else {
                LoaderKind::Fabric
            };
            client
                .install_loader(loader, &key.mc_version, version, &list, assets, None)
                .await
        }
        _ => client.install_version(entry, &list, assets, None).await,
    }
    .map_err(|e| format!("runtime fetch: {e}"))?;

    if !staging
        .join("versions")
        .join(&report.version)
        .join(format!("{}.jar", report.version))
        .is_file()
    {
        return Err(format!(
            "runtime fetch finished without a client jar for '{}'",
            report.version
        ));
    }

    // The profile's own `javaVersion.majorVersion` wins over the table —
    // that is what lets a 1.7.10 pack and a 1.20.5 pack each name their Java.
    let profile_java = client
        .resolved_profile(&report.version)
        .ok()
        .and_then(|merged| merged.java_major);
    drop(client);

    let cas = state.runtimes.cas_dir();
    let staging_for_pack = staging.clone();
    let key_for_pack = key.clone();
    let manifest =
        tokio::task::spawn_blocking(move || pack_tree(&staging_for_pack, &cas, &key_for_pack))
            .await
            .map_err(|e| format!("runtime pack task: {e}"))??;

    let dir = state.runtimes.root.join(&id);
    fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let body = serde_json::to_vec_pretty(&manifest).map_err(|e| e.to_string())?;
    write_atomic(&dir.join("manifest.json"), &body).map_err(|e| e.to_string())?;
    Ok((manifest, profile_java))
}

/// Hash + zstd every staged runtime file into the shared CAS and describe the
/// result with the same manifest the pack builder writes, so clients can sync
/// a runtime with the ordinary `SyncEngine`.
fn pack_tree(staging: &Path, cas: &Path, key: &RuntimeKey) -> Result<PackManifest, String> {
    let mut files: BTreeMap<String, ManifestEntry> = BTreeMap::new();
    for top in RUNTIME_DIRS {
        let base = staging.join(top);
        if !base.is_dir() {
            continue;
        }
        for walked in WalkDir::new(&base).follow_links(false).into_iter() {
            let Ok(entry) = walked else { continue };
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            // Half-written downloads are renamed from `.part` (and CAS writes
            // from `.tmp`): never reference a partial file.
            let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
            if matches!(ext, "part" | "tmp") {
                continue;
            }
            let rel = path
                .strip_prefix(staging)
                .map_err(|e| e.to_string())?
                .to_string_lossy()
                .replace('\\', "/");
            let bytes = fs::read(path).map_err(|e| format!("read {rel}: {e}"))?;
            let hash = blake3::hash(&bytes).to_hex().to_string();
            let payload = zstd::encode_all(&bytes[..], COMPRESSION_LEVEL)
                .map_err(|e| format!("compress {rel}: {e}"))?;
            let blob = cas.join(&hash[..2]).join(&hash);
            if !blob.is_file() {
                if let Some(parent) = blob.parent() {
                    fs::create_dir_all(parent).map_err(|e| format!("mkdir {rel}: {e}"))?;
                }
                let tmp = blob.with_extension("tmp");
                fs::write(&tmp, &payload).map_err(|e| format!("write {rel}: {e}"))?;
                fs::rename(&tmp, &blob).map_err(|e| format!("rename {rel}: {e}"))?;
            }
            files.insert(
                rel,
                ManifestEntry {
                    hash,
                    size: bytes.len() as u64,
                    compressed_size: Some(payload.len() as u64),
                    user_override: false,
                    executable: false,
                },
            );
        }
    }

    if files.is_empty() {
        return Err("runtime tree is empty — nothing was fetched".to_string());
    }

    Ok(PackManifest {
        pack_id: key.id(),
        pack_version: key.install_version(),
        game_version: key.mc_version.clone(),
        loader: key.loader_label(),
        created_at: chrono::Utc::now(),
        files,
        user_overrides: Vec::new(),
    })
}

// ----------------------------------------------------------------- lifecycle

/// Drop runtime records nothing references, then drop CAS blobs no remaining
/// runtime references. Never touches pack distributions, and does nothing at
/// all while a build is in flight (its blobs are not in any manifest yet).
fn sweep(state: &Arc<AppState>) -> Result<Value, ApiError> {
    let packs = PackRegistry::load(&state.data_dir)?;
    let mut registry = state.runtimes.load_registry()?;

    if registry
        .runtimes
        .iter()
        .any(|r| r.status == RuntimeStatus::Building)
    {
        return Ok(json!({
            "skipped": "a runtime build is in progress",
            "dropped_runtimes": [], "kept_runtimes": [],
            "dropped_blobs": 0, "reclaimed_bytes": 0,
        }));
    }

    let live_ids: Vec<String> = registry
        .runtimes
        .iter()
        .filter(|r| r.status == RuntimeStatus::Ready)
        .filter(|r| !RuntimeRegistry::referenced_by(&packs, &r.id).is_empty())
        .map(|r| r.id.clone())
        .collect();

    let mut dropped_runtimes = Vec::new();
    let stale: Vec<String> = registry
        .runtimes
        .iter()
        .map(|r| r.id.clone())
        .filter(|id| !live_ids.contains(id))
        .collect();
    for id in stale {
        if let Some(entry) = registry.remove(&id) {
            let _ = fs::remove_file(state.runtimes.manifest_path(&id));
            let _ = fs::remove_dir(state.runtimes.root.join(&id));
            dropped_runtimes.push(entry.id);
        }
    }
    state.runtimes.save_registry(&registry)?;

    // Orphan blobs: anything the kept manifests do not name.
    let mut live_hashes: HashSet<String> = HashSet::new();
    for entry in &registry.runtimes {
        if let Some(manifest) = state.runtimes.read_manifest(&entry.id) {
            for file in manifest.files.values() {
                live_hashes.insert(file.hash.clone());
            }
        }
    }
    let mut dropped_blobs = 0u64;
    let mut reclaimed = 0u64;
    let cas = state.runtimes.cas_dir();
    let io_err = |e: std::io::Error| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("runtime gc: {e}"),
        )
    };
    if cas.is_dir() {
        for shard in fs::read_dir(&cas).map_err(io_err)? {
            let shard = shard.map_err(io_err)?;
            let path = shard.path();
            if !path.is_dir() {
                continue;
            }
            for object in fs::read_dir(&path).map_err(io_err)? {
                let object = object.map_err(io_err)?;
                let name = object.file_name().to_string_lossy().into_owned();
                let target = path.join(&name);
                if !target.is_file() {
                    continue; // in-flight `.tmp` writes belong to a builder
                }
                if name.len() == 64 && live_hashes.contains(&name) {
                    continue;
                }
                let size = fs::metadata(&target).map(|m| m.len()).unwrap_or(0);
                match fs::remove_file(&target) {
                    Ok(()) => {
                        dropped_blobs += 1;
                        reclaimed += size;
                    }
                    Err(e) => tracing::warn!("runtime gc: cannot remove {name}: {e}"),
                }
            }
            let _ = fs::remove_dir(&path); // shards that just emptied
        }
    }

    Ok(json!({
        "dropped_runtimes": dropped_runtimes,
        "kept_runtimes": registry.runtimes.iter()
            .filter(|r| r.status == RuntimeStatus::Ready)
            .map(|r| r.id.clone())
            .collect::<Vec<_>>(),
        "dropped_blobs": dropped_blobs,
        "reclaimed_bytes": reclaimed,
    }))
}

// ------------------------------------------------------------------- json

fn entry_json(entry: &RuntimeEntry, referenced_by: Vec<String>) -> Value {
    json!({
        "id": entry.id,
        "game_version": entry.mc_version,
        "loader": entry.loader,
        "loader_version": entry.loader_version,
        "install_version": entry.install_version,
        "status": entry.status,
        "assets": if entry.assets { "full" } else { "index" },
        "error": entry.error,
        "created_at": entry.created_at,
        "updated_at": entry.updated_at,
        "file_count": entry.file_count,
        "total_bytes": entry.total_bytes,
        "stored_bytes": entry.stored_bytes,
        "required_java": entry.required_java,
        "referenced_by": referenced_by,
    })
}

// --------------------------------------------------------------- handlers

/// `GET /runtimes` — ready runtimes for launcher clients (no auth, like `/packs`).
pub async fn client_runtime_list(
    State(state): State<Arc<AppState>>,
) -> Result<Json<Value>, ApiError> {
    let registry = state.runtimes.load_registry()?;
    let runtimes: Vec<Value> = registry
        .runtimes
        .iter()
        .filter(|r| r.status == RuntimeStatus::Ready)
        .map(|r| {
            json!({
                "id": r.id,
                "game_version": r.mc_version,
                "loader": r.loader,
                "loader_version": r.loader_version,
                "install_version": r.install_version,
                "assets": r.assets,
                "file_count": r.file_count,
                "total_bytes": r.total_bytes,
            })
        })
        .collect();
    Ok(Json(json!({ "runtimes": runtimes })))
}

/// `GET /runtimes/:id/manifest.json`
pub async fn runtime_manifest(
    State(state): State<Arc<AppState>>,
    UrlPath(id): UrlPath<String>,
) -> Result<impl IntoResponse, ApiError> {
    let bytes = fs::read(state.runtimes.manifest_path(&id))
        .map_err(|_| ApiError::new(StatusCode::NOT_FOUND, format!("no runtime manifest: {id}")))?;
    Ok(([(header::CONTENT_TYPE, "application/json")], bytes))
}

/// `GET /runtimes/:id/cas/objects/:shard/:hash` — one shared runtime blob.
pub async fn runtime_blob(
    State(state): State<Arc<AppState>>,
    UrlPath((id, shard, hash)): UrlPath<(String, String, String)>,
) -> Result<impl IntoResponse, ApiError> {
    // Same rule as the pack blobs: the shard must be the hash's own prefix and
    // the hash plain lowercase hex, so the path cannot escape the CAS store.
    let valid = hash.len() == 64
        && hash
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase());
    if !valid || shard != hash[..2] {
        return Err(ApiError::bad_request("invalid CAS object reference"));
    }
    if !valid_runtime_id(&id) {
        return Err(ApiError::bad_request("invalid runtime id"));
    }
    let path = state.runtimes.blob_path(&hash);
    let bytes = fs::read(&path)
        .map_err(|_| ApiError::new(StatusCode::NOT_FOUND, "CAS object not found"))?;
    Ok(([(header::CONTENT_TYPE, "application/octet-stream")], bytes))
}

fn valid_runtime_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 200
        && !id.contains('/')
        && !id.contains('\\')
        && !id.starts_with('.')
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '+'))
}

#[derive(Debug, Deserialize)]
pub struct ConsoleRuntimeBody {
    pub game_version: String,
    #[serde(default)]
    pub loader: Option<String>,
    #[serde(default)]
    pub loader_version: Option<String>,
    /// Fetch every asset object too (large: hundreds of MB).
    #[serde(default)]
    pub assets: Option<bool>,
    /// Rebuild even when a ready runtime already exists.
    #[serde(default)]
    pub force: Option<bool>,
}

/// `GET /console/runtimes` — every record, with pack references.
pub async fn console_list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    crate::console::auth(&state, &headers)?;
    let packs = PackRegistry::load(&state.data_dir)?;
    let registry = state.runtimes.load_registry()?;
    let runtimes: Vec<Value> = registry
        .runtimes
        .iter()
        .map(|r| entry_json(r, RuntimeRegistry::referenced_by(&packs, &r.id)))
        .collect();
    Ok(Json(json!({
        "runtimes": runtimes,
        "default_assets": state.runtime_assets,
        "cas_bytes": cas_size(&state.runtimes.cas_dir()),
    })))
}

/// `POST /console/runtimes` — queue a runtime build.
pub async fn console_build(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<ConsoleRuntimeBody>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    crate::console::auth(&state, &headers)?;
    let key = RuntimeKey {
        mc_version: body.game_version.trim().to_string(),
        loader: body
            .loader
            .as_deref()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(|l| l.to_ascii_lowercase()),
        loader_version: body
            .loader_version
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_string),
    };
    let result = schedule(&state, &key, body.assets, body.force.unwrap_or(false)).await?;
    let code = if result["status"] == json!("building") {
        StatusCode::ACCEPTED
    } else {
        StatusCode::OK
    };
    Ok((code, Json(result)))
}

/// `DELETE /console/runtimes/:id?force=1` — drop the record and its manifest.
/// Shared blobs stay until `gc`, because other runtimes may reference them.
pub async fn console_delete(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    UrlPath(id): UrlPath<String>,
    Query(q): Query<BTreeMap<String, String>>,
) -> Result<Json<Value>, ApiError> {
    crate::console::auth(&state, &headers)?;
    if !valid_runtime_id(&id) {
        return Err(ApiError::bad_request("invalid runtime id"));
    }
    let force = q
        .get("force")
        .map(|v| v == "1" || v == "true")
        .unwrap_or(false);

    let _guard = state.runtime_lock.lock().await;
    let packs = PackRegistry::load(&state.data_dir)?;
    let mut registry = state.runtimes.load_registry()?;
    let entry = registry
        .remove(&id)
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, format!("no runtime: {id}")))?;
    let referenced = RuntimeRegistry::referenced_by(&packs, &id);
    if !referenced.is_empty() && !force {
        registry.upsert(entry);
        return Err(ApiError::bad_request(format!(
            "runtime '{id}' is used by pack(s) {}; pass force=1 to remove it anyway",
            referenced.join(", ")
        )));
    }
    let _ = fs::remove_file(state.runtimes.manifest_path(&id));
    let _ = fs::remove_dir(state.runtimes.root.join(&id));
    state.runtimes.save_registry(&registry)?;
    drop(_guard);

    tracing::info!("removed runtime '{id}' from the registry (shared CAS blobs kept for gc)");
    Ok(Json(json!({ "removed": id, "referenced_by": referenced })))
}

/// `POST /console/runtimes/gc` — reclaim unreferenced runtimes and blobs.
pub async fn console_gc(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    crate::console::auth(&state, &headers)?;
    let _guard = state.runtime_lock.lock().await;
    let report = sweep(&state)?;
    tracing::info!(
        "runtime gc: {} runtime(s), {} blob(s), {} byte(s) reclaimed",
        report["dropped_runtimes"]
            .as_array()
            .map(|a| a.len())
            .unwrap_or(0),
        report["dropped_blobs"].as_u64().unwrap_or(0),
        report["reclaimed_bytes"].as_u64().unwrap_or(0),
    );
    Ok(Json(report))
}

fn cas_size(cas: &Path) -> u64 {
    WalkDir::new(cas)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| e.metadata().ok().map(|m| m.len()))
        .sum()
}

/// Record a runtime for an imported pack, queuing the build when needed.
/// Failures are logged and swallowed: a pack that imports fine must not be
/// lost because the runtime store could not reach Mojang, and Forge/NeoForge
/// packs keep shipping their installer jar through `src/loader/` instead.
pub async fn attach(
    state: &Arc<AppState>,
    pack_id: &str,
    loader: &str,
    game_version: &str,
) -> Option<String> {
    let key = key_for_plan(loader, game_version)?;
    if !key.is_buildable() {
        tracing::debug!(
            "pack '{pack_id}': loader '{}' has no server-side runtime build (needs an installer)",
            key.loader_label().unwrap_or_default()
        );
        return None;
    }
    let fallback = key.id();
    let id = match schedule(state, &key, None, false).await {
        Ok(value) => value["id"].as_str().unwrap_or(&fallback).to_string(),
        Err(e) => {
            tracing::warn!("pack '{pack_id}': no server-side runtime for '{fallback}': {e}");
            return None;
        }
    };

    let _guard = state.runtime_lock.lock().await;
    match PackRegistry::load(&state.data_dir) {
        Ok(mut registry) => {
            if let Some(entry) = registry.packs.iter_mut().find(|p| p.id == pack_id) {
                entry.runtime_id = Some(id.clone());
                if let Err(e) = registry.save(&state.data_dir) {
                    tracing::warn!("pack '{pack_id}': could not record runtime '{id}': {e}");
                }
            }
        }
        Err(e) => tracing::warn!("pack '{pack_id}': could not load the registry: {e}"),
    }
    Some(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_stable_and_readable() {
        let vanilla = RuntimeKey::vanilla("1.20.1");
        assert_eq!(vanilla.id(), "1.20.1--vanilla");
        assert_eq!(vanilla.install_version(), "1.20.1");
        assert_eq!(vanilla.loader_label(), None);

        let fabric = RuntimeKey {
            mc_version: "1.20.1".into(),
            loader: Some("fabric".into()),
            loader_version: Some("0.15.11".into()),
        };
        assert_eq!(fabric.id(), "1.20.1--fabric--0.15.11");
        assert_eq!(fabric.install_version(), "1.20.1-fabric-0.15.11");
        assert_eq!(fabric.loader_label(), Some("fabric-0.15.11".into()));
    }

    #[test]
    fn plan_loaders_map_to_keys() {
        assert_eq!(
            key_for_plan("", "1.20.1"),
            Some(RuntimeKey::vanilla("1.20.1"))
        );
        assert_eq!(
            key_for_plan("fabric-0.15.11", "1.20.1").map(|k| k.id()),
            Some("1.20.1--fabric--0.15.11".into())
        );
        assert_eq!(
            key_for_plan("quilt-0.24.0", "1.20.4").map(|k| k.id()),
            Some("1.20.4--quilt--0.24.0".into())
        );
        // The mapping is faithful even for loaders the server cannot build:
        // `attach` skips those, `schedule` refuses them outright.
        let forge = key_for_plan("forge-47.2.0", "1.20.1");
        assert_eq!(
            forge.as_ref().map(|k| k.id()),
            Some("1.20.1--forge--47.2.0".into())
        );
        assert_eq!(forge.map(|k| k.is_buildable()), Some(false));
        // Unknown loaders and missing game versions map to no runtime at all.
        assert_eq!(key_for_plan("liteloader-1.12", "1.12.2"), None);
        assert_eq!(key_for_plan("fabric-0.15.11", ""), None);
    }

    #[test]
    fn unsafe_keys_are_rejected() {
        let nasty = RuntimeKey {
            mc_version: "../etc".into(),
            loader: None,
            loader_version: None,
        };
        assert!(!nasty.is_safe());
        let loader = RuntimeKey {
            mc_version: "1.20.1".into(),
            loader: Some("fabric".into()),
            loader_version: Some("../../x".into()),
        };
        assert!(!loader.is_safe());
    }

    #[test]
    fn forge_and_neoforge_are_not_buildable() {
        let forge = RuntimeKey {
            mc_version: "1.20.1".into(),
            loader: Some("forge".into()),
            loader_version: Some("47.2.0".into()),
        };
        assert!(forge.is_safe());
        assert!(!forge.is_buildable());
    }

    #[test]
    fn ids_survive_the_blob_and_url_checks() {
        assert!(valid_runtime_id("1.20.1--fabric--0.15.11"));
        assert!(valid_runtime_id("1.21.4--vanilla"));
        assert!(!valid_runtime_id("../etc"));
        assert!(!valid_runtime_id("a/b"));
    }

    #[test]
    fn registry_upsert_keeps_creation_time() {
        let mut registry = RuntimeRegistry::default();
        let base = RuntimeEntry {
            id: "1.20.1--vanilla".into(),
            mc_version: "1.20.1".into(),
            loader: None,
            loader_version: None,
            install_version: "1.20.1".into(),
            status: RuntimeStatus::Building,
            assets: false,
            error: None,
            created_at: "first".into(),
            updated_at: "first".into(),
            file_count: 0,
            total_bytes: 0,
            stored_bytes: 0,
            required_java: Some(17),
        };
        registry.upsert(base.clone());
        registry.upsert(RuntimeEntry {
            status: RuntimeStatus::Ready,
            updated_at: "second".into(),
            created_at: "second".into(),
            ..base
        });
        assert_eq!(registry.runtimes.len(), 1);
        assert_eq!(registry.runtimes[0].created_at, "first");
        assert_eq!(registry.runtimes[0].status, RuntimeStatus::Ready);
    }
}
