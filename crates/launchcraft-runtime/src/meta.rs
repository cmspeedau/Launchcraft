//! Minecraft metadata + file acquisition, modeled on PrismLauncher's
//! pipeline: fetch the Mojang version manifest, download a version profile
//! (following `inheritsFrom` chains, e.g. a fabric overlay onto its vanilla
//! base), the client jar, the asset index and every asset object, and all
//! applicable library jars — then turn the installed tree into a concrete
//! `LaunchConfig` for the process spawner.
//!
//! Storage mirrors the standard launcher layout under `LauncherPaths`:
//! `versions/<id>/{<id>.json,<id>.jar}`, `assets/{indexes,objects}`, and a
//! shared Maven-layout `libraries/` directory. Downloads are sha1-checked
//! (when the metadata provides one) and skipped when already present.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::Read as _;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use futures_util::StreamExt as _;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha1::{Digest, Sha1};

use launchcraft_core::paths::LauncherPaths;

use crate::error::RuntimeError;
use crate::mojang::{
    AssetIndex, AssetIndexRef, DownloadInfo, GameArg, GameArgs, Library, LibraryDownloads,
    MergedProfile, Rule, RuleAction, VersionManifest,
};
use crate::platform::{OperatingSystem, PlatformInfo};
use crate::process::LaunchConfig;

pub const DEFAULT_META_URL: &str = "https://piston-meta.mojang.com";
pub const DEFAULT_FABRIC_URL: &str = "https://meta.fabricmc.net/v2";
pub const DEFAULT_QUILT_URL: &str = "https://meta.quiltmc.org/v3";
pub const DEFAULT_RESOURCES_URL: &str = "https://resources.download.minecraft.net";

/// Hard cap on any single download (metadata lies and hosts mislead).
const MAX_DOWNLOAD_BYTES: u64 = 512 * 1024 * 1024;

// -------------------------------------------------------------- version list

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LatestRef {
    pub release: String,
    pub snapshot: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VersionListEntry {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub url: String,
    #[serde(default)]
    pub time: Option<String>,
    #[serde(default, rename = "releaseTime")]
    pub release_time: Option<String>,
}

/// `version_manifest_v2.json` as served by piston-meta.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VersionListFile {
    pub latest: LatestRef,
    pub versions: Vec<VersionListEntry>,
}

impl VersionListFile {
    pub fn find(&self, id: &str) -> Option<&VersionListEntry> {
        self.versions.iter().find(|v| v.id == id)
    }
}

// ----------------------------------------------------------------- progress

#[derive(Debug, Clone)]
pub struct InstallProgress {
    /// Coarse stage: "profile", "client jar", "assets", "libraries", "done".
    pub phase: &'static str,
    pub done: u64,
    pub total: u64,
    /// Current item (file/object name), best effort.
    pub item: String,
}

pub type Progress = Arc<dyn Fn(InstallProgress) + Send + Sync>;

#[derive(Debug, Clone, Default)]
pub struct InstallReport {
    pub version: String,
    pub files_written: u64,
    pub bytes_downloaded: u64,
    pub files_skipped: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoaderKind {
    Fabric,
    Quilt,
}

impl LoaderKind {
    pub fn id(self) -> &'static str {
        match self {
            LoaderKind::Fabric => "fabric",
            LoaderKind::Quilt => "quilt",
        }
    }
}

// --------------------------------------------------------------------- client

pub struct MinecraftClient {
    http: Client,
    meta_base: String,
    fabric_base: String,
    quilt_base: String,
    resources_base: String,
    paths: LauncherPaths,
}

impl MinecraftClient {
    /// Production client; bases honour `LAUNCHCRAFT_META_URL`,
    /// `LAUNCHCRAFT_FABRIC_URL`, `LAUNCHCRAFT_QUILT_URL` and
    /// `LAUNCHCRAFT_RESOURCES_URL`.
    pub fn new(paths: LauncherPaths) -> Self {
        let base = |var: &str, default: &str| {
            std::env::var(var)
                .ok()
                .filter(|u| !u.is_empty())
                .unwrap_or_else(|| default.to_string())
        };
        Self::with_urls(
            paths,
            base("LAUNCHCRAFT_META_URL", DEFAULT_META_URL),
            base("LAUNCHCRAFT_FABRIC_URL", DEFAULT_FABRIC_URL),
            base("LAUNCHCRAFT_QUILT_URL", DEFAULT_QUILT_URL),
            base("LAUNCHCRAFT_RESOURCES_URL", DEFAULT_RESOURCES_URL),
        )
    }

    pub fn with_urls(
        paths: LauncherPaths,
        meta_base: String,
        fabric_base: String,
        quilt_base: String,
        resources_base: String,
    ) -> Self {
        Self {
            http: Client::builder()
                .user_agent(concat!("Launchcraft/", env!("CARGO_PKG_VERSION")))
                .build()
                .unwrap_or_default(),
            meta_base: trim(meta_base),
            fabric_base: trim(fabric_base),
            quilt_base: trim(quilt_base),
            resources_base: trim(resources_base),
            paths,
        }
    }

    pub fn paths(&self) -> &LauncherPaths {
        &self.paths
    }

    async fn get_bytes(&self, url: &str) -> Result<Vec<u8>, RuntimeError> {
        let mut res = self.http.get(url).send().await?;
        if !res.status().is_success() {
            return Err(RuntimeError::ManifestError(format!(
                "download failed ({}): {url}",
                res.status()
            )));
        }
        let mut buf: Vec<u8> = Vec::new();
        while let Some(chunk) = res.chunk().await? {
            if buf.len() as u64 + chunk.len() as u64 > MAX_DOWNLOAD_BYTES {
                return Err(RuntimeError::ManifestError(format!(
                    "download exceeds the {} MB cap: {url}",
                    MAX_DOWNLOAD_BYTES / (1024 * 1024)
                )));
            }
            buf.extend_from_slice(&chunk);
        }
        Ok(buf)
    }

    pub async fn version_list(&self) -> Result<VersionListFile, RuntimeError> {
        let url = format!("{}/mc/game/version_manifest_v2.json", self.meta_base);
        let bytes = self.get_bytes(&url).await?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    /// Loader versions published for `mc_version` (newest first).
    pub async fn loader_versions(
        &self,
        kind: LoaderKind,
        mc_version: &str,
    ) -> Result<Vec<String>, RuntimeError> {
        if !safe_version_id(mc_version) {
            return Err(RuntimeError::ManifestError(format!(
                "unsafe version id: {mc_version}"
            )));
        }
        let (base, route) = match kind {
            LoaderKind::Fabric => (&self.fabric_base, "minecraft"),
            LoaderKind::Quilt => (&self.quilt_base, "game"),
        };
        let url = format!("{base}/versions/loader/{route}/{mc_version}");
        let bytes = self.get_bytes(&url).await?;
        let values: Vec<Value> = serde_json::from_slice(&bytes)?;
        Ok(values
            .iter()
            .filter_map(|v| {
                v.pointer("/loader/version")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .collect())
    }

    // ---------------------------------------------------------------- install

    /// Download a self-contained (vanilla) version: profile, client jar,
    /// assets and libraries.
    pub async fn install_version(
        &self,
        entry: &VersionListEntry,
        list: &VersionListFile,
        with_assets: bool,
        progress: Option<&Progress>,
    ) -> Result<InstallReport, RuntimeError> {
        report(progress, "profile", 0, 1, &entry.id);
        let bytes = self.get_bytes(&entry.url).await?;
        let profile: VersionManifest = serde_json::from_slice(&bytes)?;
        if profile.id != entry.id {
            return Err(RuntimeError::ManifestError(format!(
                "version manifest promises '{}' but delivered '{}'",
                entry.id, profile.id
            )));
        }
        self.write_version_json(&profile.id, &bytes)?;
        let layers = self.load_chain(profile, list).await?;
        let id = layers.last().expect("non-empty chain").id.clone();
        self.finish_install(&layers, &id, with_assets, progress)
            .await
    }

    /// Install a fabric/quilt loader over an already-listed vanilla version:
    /// fetch the loader profile, normalize its library list into concrete
    /// download entries, and pull jar/assets/libraries like any version.
    pub async fn install_loader(
        &self,
        kind: LoaderKind,
        mc_version: &str,
        loader_version: &str,
        list: &VersionListFile,
        with_assets: bool,
        progress: Option<&Progress>,
    ) -> Result<InstallReport, RuntimeError> {
        if !safe_version_id(mc_version) || !safe_version_id(loader_version) {
            return Err(RuntimeError::ManifestError(format!(
                "unsafe version/loader id: {mc_version} / {loader_version}"
            )));
        }
        let base = match kind {
            LoaderKind::Fabric => &self.fabric_base,
            LoaderKind::Quilt => &self.quilt_base,
        };
        let url = format!("{base}/versions/loader/{mc_version}/{loader_version}/profile/json");
        report(progress, "profile", 0, 1, loader_version);
        let bytes = self.get_bytes(&url).await?;
        let raw: Value = serde_json::from_slice(&bytes)
            .map_err(|e| RuntimeError::ManifestError(format!("bad loader profile: {e}")))?;
        let mut profile: VersionManifest = serde_json::from_value(raw.clone())?;

        // Loader metadata spells libraries several ways; normalize them all
        // into explicit artifact download entries so install and launch can
        // treat every layer uniformly.
        let libs_raw = raw.get("libraries").and_then(Value::as_array).cloned();
        normalize_loader_libraries(&mut profile.libraries, libs_raw.as_deref());

        profile.id = format!("{mc_version}-{}-{loader_version}", kind.id());
        profile.inherits_from = Some(mc_version.to_string());
        self.write_version_json(
            &profile.id,
            &serde_json::to_vec_pretty(&profile)
                .map_err(|e| RuntimeError::ManifestError(e.to_string()))?,
        )?;

        let layers = self.load_chain(profile, list).await?;
        let id = layers.last().expect("non-empty chain").id.clone();
        self.finish_install(&layers, &id, with_assets, progress)
            .await
    }

    /// Walk the `inheritsFrom` chain upward, loading (and caching) each
    /// parent profile from local disk or its manifest URL.
    async fn load_chain(
        &self,
        child: VersionManifest,
        list: &VersionListFile,
    ) -> Result<Vec<VersionManifest>, RuntimeError> {
        let mut chain = vec![child];
        while let Some(parent_id) = chain.last().and_then(|p| p.inherits_from.clone()) {
            if chain.iter().any(|l| l.id == parent_id) {
                return Err(RuntimeError::ManifestError(format!(
                    "inheritsFrom cycle at '{parent_id}'"
                )));
            }
            let profile = match self.read_version_json(&parent_id)? {
                Some(local) => local,
                None => {
                    let entry = list.find(&parent_id).ok_or_else(|| {
                        RuntimeError::ManifestError(format!(
                            "parent version '{parent_id}' is not in the version manifest"
                        ))
                    })?;
                    let bytes = self.get_bytes(&entry.url).await?;
                    self.write_version_json(&entry.id, &bytes)?;
                    serde_json::from_slice(&bytes)?
                }
            };
            chain.push(profile);
        }
        chain.reverse(); // base first
        Ok(chain)
    }

    async fn finish_install(
        &self,
        layers: &[VersionManifest],
        install_id: &str,
        with_assets: bool,
        progress: Option<&Progress>,
    ) -> Result<InstallReport, RuntimeError> {
        let merged = MergedProfile::merge(layers)
            .ok_or_else(|| RuntimeError::ManifestError("empty version chain".into()))?;
        let platform = PlatformInfo::current();
        let mut result = InstallReport {
            version: install_id.to_string(),
            ..Default::default()
        };

        // Client jar: every install (including overlays) gets its own copy
        // named after the install id — the standard launcher convention.
        if let Some(client) = merged.client_download.clone() {
            report(progress, "client jar", 0, 1, install_id);
            let jar = self
                .version_dir(install_id)
                .join(format!("{install_id}.jar"));
            match self
                .fetch_download(&client.url, jar, client.sha1.as_deref(), client.size)
                .await?
            {
                Downloaded::New(bytes) => {
                    result.bytes_downloaded += bytes;
                    result.files_written += 1;
                }
                Downloaded::Skipped => result.files_skipped += 1,
            }
        }

        if with_assets {
            if let Some(idx) = merged.asset_index.clone() {
                let (written, skipped, bytes) = self.install_assets(&idx, progress).await?;
                result.files_written += written;
                result.files_skipped += skipped;
                result.bytes_downloaded += bytes;
            }
        }

        // Libraries (rules filtered) plus the natives classifier jars.
        /// (url, repo-relative path, sha1, size, display name)
        type LibJob = (String, String, Option<String>, Option<u64>, String);
        let mut jobs: Vec<LibJob> = Vec::new();
        for lib in &merged.libraries {
            if !lib.is_applicable(&platform) {
                continue;
            }
            let Some(dl) = &lib.downloads else { continue };
            if let Some(a) = dl.artifact.as_ref() {
                if let Some(rel) = &a.path {
                    jobs.push((
                        a.url.clone(),
                        rel.clone(),
                        a.sha1.clone(),
                        a.size,
                        basename(rel),
                    ));
                }
            }
            if let Some(classifier) = lib.natives_classifier(&platform) {
                if let Some(c) = dl.classifiers.as_ref().and_then(|c| c.get(&classifier)) {
                    if let Some(rel) = &c.path {
                        jobs.push((
                            c.url.clone(),
                            rel.clone(),
                            c.sha1.clone(),
                            c.size,
                            basename(rel),
                        ));
                    }
                }
            }
        }
        let total = jobs.len() as u64;
        report(progress, "libraries", 0, total, "");
        let mut done = 0u64;
        for (url, rel, sha1, size, item) in jobs {
            let dest = self.paths.libraries.join(&rel);
            match self
                .fetch_download(&url, dest, sha1.as_deref(), size)
                .await?
            {
                Downloaded::New(bytes) => {
                    result.bytes_downloaded += bytes;
                    result.files_written += 1;
                }
                Downloaded::Skipped => result.files_skipped += 1,
            }
            done += 1;
            report(progress, "libraries", done, total, &item);
        }

        report(progress, "done", 1, 1, install_id);
        Ok(result)
    }

    // ---------------------------------------------------------------- assets

    async fn install_assets(
        &self,
        idx: &AssetIndexRef,
        progress: Option<&Progress>,
    ) -> Result<(u64, u64, u64), RuntimeError> {
        let index_path = self
            .paths
            .assets
            .join("indexes")
            .join(format!("{}.json", idx.id));
        let bytes = if index_path.is_file() {
            std::fs::read(&index_path)?
        } else {
            let bytes = self.get_bytes(&idx.url).await?;
            verify_sha1(&bytes, &idx.sha1, &idx.id)?;
            if let Some(parent) = index_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&index_path, &bytes)?;
            bytes
        };
        let index: AssetIndex = serde_json::from_slice(&bytes)?;
        let total = index.objects.len() as u64;
        report(progress, "assets", 0, total, "");

        let mut written = 0u64;
        let mut skipped = 0u64;
        let mut bytes_downloaded = 0u64;
        // Objects are content-addressed and tiny: hash-check on write,
        // size-match on skip; fan the downloads out modestly.
        let futures = index
            .objects
            .values()
            .map(|obj| {
                let this = self;
                let hash = obj.hash.clone();
                let size = obj.size;
                async move {
                    let url = format!(
                        "{}/{}/{}",
                        self.resources_base,
                        &hash[..2.min(hash.len())],
                        hash
                    );
                    let outcome = this
                        .fetch_download(&url, this.object_path(&hash), Some(&hash), Some(size))
                        .await?;
                    Ok::<_, RuntimeError>(outcome)
                }
            })
            .collect::<Vec<_>>();
        let mut stream = futures_util::stream::iter(futures).buffered(16);
        let mut seen = 0u64;
        while let Some(outcome) = stream.next().await {
            match outcome? {
                Downloaded::New(bytes) => {
                    written += 1;
                    bytes_downloaded += bytes;
                }
                Downloaded::Skipped => skipped += 1,
            }
            seen += 1;
            if seen.is_multiple_of(64) || seen == total {
                report(progress, "assets", seen, total, "");
            }
        }
        Ok((written, skipped, bytes_downloaded))
    }

    // --------------------------------------------------------------- launch

    pub fn is_installed(&self, version_id: &str) -> bool {
        let dir = self.version_dir(version_id);
        dir.join(format!("{version_id}.jar")).is_file()
            && dir.join(format!("{version_id}.json")).is_file()
    }

    /// Version ids that have both their profile json and client jar.
    pub fn installed_ids(&self) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(&self.paths.versions) else {
            return Vec::new();
        };
        let mut ids: Vec<String> = entries
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                if e.path().is_dir() && self.is_installed(&name) {
                    Some(name)
                } else {
                    None
                }
            })
            .collect();
        ids.sort();
        ids
    }

    /// Read the installed chain for `version_id` (json must exist for every
    /// layer) and build the merged picture.
    pub fn resolved_profile(&self, version_id: &str) -> Result<MergedProfile, RuntimeError> {
        let mut chain = Vec::new();
        let mut current = version_id.to_string();
        loop {
            let profile = self.read_version_json(&current)?.ok_or_else(|| {
                RuntimeError::ManifestError(format!("version '{current}' is not installed"))
            })?;
            let parent = profile.inherits_from.clone();
            chain.push(profile);
            match parent {
                Some(p) => current = p,
                None => break,
            }
        }
        chain.reverse();
        MergedProfile::merge(&chain)
            .ok_or_else(|| RuntimeError::ManifestError(format!("no layers for '{version_id}'")))
    }

    /// Turn an installed version + instance + identity into a runnable
    /// launch configuration (classpath, natives, substituted game args).
    pub fn plan_launch(
        &self,
        spec: &LaunchSpec<'_>,
        platform: &PlatformInfo,
    ) -> Result<LaunchPlan, RuntimeError> {
        let merged = self.resolved_profile(spec.version_id)?;
        let game_dir = spec.instance_dir.join("minecraft");
        std::fs::create_dir_all(&game_dir)?;
        let natives_dir = spec.instance_dir.join("natives");
        let natives_written = extract_natives(
            &merged.libraries,
            platform,
            &self.paths.libraries,
            &natives_dir,
        )?;

        let client_jar = self
            .version_dir(spec.version_id)
            .join(format!("{}.jar", spec.version_id));
        let classpath = merged.resolve_classpath(&self.paths.libraries, &client_jar, platform);
        let sep = if platform.os == OperatingSystem::Windows {
            ";"
        } else {
            ":"
        };
        let classpath_joined = classpath
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(sep);

        let asset_index = merged
            .asset_index
            .as_ref()
            .map(|i| i.id.clone())
            .unwrap_or_else(|| "legacy".to_string());

        let vars: HashMap<String, String> = HashMap::from([
            ("auth_player_name".into(), spec.identity.username.clone()),
            ("version_name".into(), spec.version_id.to_string()),
            (
                "game_directory".into(),
                game_dir.to_string_lossy().into_owned(),
            ),
            (
                "assets_root".into(),
                self.paths.assets.to_string_lossy().into_owned(),
            ),
            (
                "game_assets".into(),
                self.paths.assets.to_string_lossy().into_owned(),
            ),
            ("assets_index_name".into(), asset_index.clone()),
            ("auth_uuid".into(), spec.identity.uuid.clone()),
            ("auth_access_token".into(), spec.identity.token.clone()),
            ("auth_xuid".into(), or_zero(&spec.identity.xuid)),
            ("clientid".into(), spec.identity.client_id.clone()),
            ("client_token".into(), String::new()),
            ("user_properties".into(), "{}".into()),
            ("user_type".into(), "msa".into()),
            ("version_type".into(), "Launchcraft".into()),
            (
                "natives_directory".into(),
                natives_dir.to_string_lossy().into_owned(),
            ),
            ("launcher_name".into(), "launchcraft".into()),
            (
                "launcher_version".into(),
                env!("CARGO_PKG_VERSION").to_string(),
            ),
            ("classpath".into(), classpath_joined),
            ("classpath_sep".into(), sep.to_string()),
            (
                "primary_jar".into(),
                client_jar.to_string_lossy().into_owned(),
            ),
            (
                "libraries_root".into(),
                self.paths.libraries.to_string_lossy().into_owned(),
            ),
        ]);

        let game_args = build_game_args(&merged.game_args, platform, &vars);

        let config = LaunchConfig {
            java_binary: spec.java_binary.clone(),
            min_memory_mb: spec.min_memory_mb,
            max_memory_mb: spec.max_memory_mb,
            jvm_args: spec.jvm_extra.clone(),
            game_dir,
            assets_dir: self.paths.assets.clone(),
            asset_index,
            version_id: spec.version_id.to_string(),
            main_class: merged.main_class.clone(),
            classpath,
            natives_dir: (natives_written > 0).then_some(natives_dir),
            auth_player_name: spec.identity.username.clone(),
            auth_uuid: spec.identity.uuid.clone(),
            auth_token: spec.identity.token.clone(),
            width: None,
            height: None,
            game_args,
        };
        Ok(LaunchPlan {
            config,
            java_major: merged.java_major,
            java_component: merged.java_component,
            natives_extracted: natives_written,
        })
    }

    // -------------------------------------------------------------- plumbing

    fn version_dir(&self, id: &str) -> PathBuf {
        self.paths.versions.join(id)
    }

    fn object_path(&self, hash: &str) -> PathBuf {
        self.paths
            .assets
            .join("objects")
            .join(&hash[..2.min(hash.len())])
            .join(hash)
    }

    fn write_version_json(&self, id: &str, bytes: &[u8]) -> Result<(), RuntimeError> {
        if !safe_version_id(id) {
            return Err(RuntimeError::ManifestError(format!(
                "unsafe version id: {id}"
            )));
        }
        let dir = self.version_dir(id);
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join(format!("{id}.json")), bytes)?;
        Ok(())
    }

    fn read_version_json(&self, id: &str) -> Result<Option<VersionManifest>, RuntimeError> {
        if !safe_version_id(id) {
            return Ok(None);
        }
        let path = self.version_dir(id).join(format!("{id}.json"));
        if !path.is_file() {
            return Ok(None);
        }
        let text = std::fs::read_to_string(path)?;
        Ok(Some(serde_json::from_str(&text)?))
    }

    /// Download `url` to `dest` unless the file is already there, verified
    /// by sha1 (small files, when known) or size.
    async fn fetch_download(
        &self,
        url: &str,
        dest: PathBuf,
        sha1: Option<&str>,
        size: Option<u64>,
    ) -> Result<Downloaded, RuntimeError> {
        if dest.is_file() {
            let meta = std::fs::metadata(&dest)?;
            let size_matches = size.is_none_or(|s| meta.len() == s);
            if size_matches {
                match sha1 {
                    // Cheap enough to be worth the full hash on small files;
                    // otherwise presence + size is the accepted trade.
                    Some(want) if meta.len() <= 4 * 1024 * 1024 => {
                        let bytes = std::fs::read(&dest)?;
                        if sha1_hex(&bytes).eq_ignore_ascii_case(want) {
                            return Ok(Downloaded::Skipped);
                        }
                    }
                    Some(_) => return Ok(Downloaded::Skipped),
                    None => return Ok(Downloaded::Skipped),
                }
            }
        }
        let bytes = self.get_bytes(url).await?;
        if let Some(want) = sha1 {
            verify_sha1(&bytes, want, url)?;
        }
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Write-tmp-then-rename keeps a killed download from faking a cache.
        let tmp = dest.with_extension("part");
        std::fs::write(&tmp, &bytes)?;
        std::fs::rename(&tmp, &dest)?;
        Ok(Downloaded::New(bytes.len() as u64))
    }
}

enum Downloaded {
    New(u64),
    Skipped,
}

#[derive(Debug, Clone)]
pub struct LaunchIdentity {
    pub username: String,
    pub uuid: String,
    pub token: String,
    pub xuid: String,
    pub client_id: String,
}

pub struct LaunchSpec<'a> {
    pub version_id: &'a str,
    pub instance_dir: &'a Path,
    pub java_binary: PathBuf,
    pub min_memory_mb: u32,
    pub max_memory_mb: u32,
    pub jvm_extra: Vec<String>,
    pub identity: LaunchIdentity,
}

pub struct LaunchPlan {
    pub config: LaunchConfig,
    /// Java major version the profile asks for (21 for modern versions).
    pub java_major: Option<u32>,
    pub java_component: Option<String>,
    pub natives_extracted: usize,
}

// ------------------------------------------------------------------- helpers

fn trim(url: String) -> String {
    url.trim_end_matches('/').to_string()
}

fn basename(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_string()
}

fn or_zero(s: &str) -> String {
    if s.is_empty() {
        "0".to_string()
    } else {
        s.to_string()
    }
}

fn report(progress: Option<&Progress>, phase: &'static str, done: u64, total: u64, item: &str) {
    if let Some(p) = progress {
        p(InstallProgress {
            phase,
            done,
            total,
            item: item.to_string(),
        });
    }
}

fn sha1_hex(bytes: &[u8]) -> String {
    let mut h = Sha1::new();
    h.update(bytes);
    let out = h.finalize();
    let mut s = String::with_capacity(40);
    for b in out {
        let _ = write!(s, "{b:02x}");
    }
    s
}

fn verify_sha1(bytes: &[u8], want: &str, what: &str) -> Result<(), RuntimeError> {
    if sha1_hex(bytes).eq_ignore_ascii_case(want) {
        Ok(())
    } else {
        Err(RuntimeError::ManifestError(format!(
            "sha1 mismatch for {what}"
        )))
    }
}

/// Version ids are directory names: plain readable tokens only.
pub fn safe_version_id(id: &str) -> bool {
    !id.is_empty()
        && id != "."
        && id != ".."
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'+'))
}

/// Substitute `${name}` launch placeholders from a resolved variable map.
/// Unknown placeholders resolve to empty strings so a launch still proceeds
/// (official metadata sometimes gates them behind features we skip).
pub fn substitute_args(template: &str, vars: &HashMap<String, String>) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after.find('}') {
            Some(end) => {
                let key = &after[..end];
                out.push_str(vars.get(key).map(String::as_str).unwrap_or(""));
                rest = &after[end + 1..];
            }
            None => {
                out.push_str(&rest[start..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

/// Render the game's own argument list from merged profile data.
pub fn build_game_args(
    spec: &GameArgs,
    platform: &PlatformInfo,
    vars: &HashMap<String, String>,
) -> Vec<String> {
    let mut out = Vec::new();
    match spec {
        GameArgs::Modern(entries) => {
            for entry in entries {
                match entry {
                    GameArg::Plain(s) => out.push(substitute_args(s, vars)),
                    GameArg::Conditional { rules, value } => {
                        if !rules_allow(rules, platform) {
                            continue;
                        }
                        match value {
                            crate::mojang::ArgValue::One(s) => out.push(substitute_args(s, vars)),
                            crate::mojang::ArgValue::Many(vs) => {
                                out.extend(vs.iter().map(|s| substitute_args(s, vars)))
                            }
                        }
                    }
                }
            }
        }
        GameArgs::Legacy(raw) => {
            out.extend(raw.split_whitespace().map(|s| substitute_args(s, vars)));
        }
    }
    out
}

/// Same allow/deny folding the library rules use.
fn rules_allow(rules: &[Rule], platform: &PlatformInfo) -> bool {
    let mut allowed = false;
    for rule in rules {
        if rule.matches(platform) {
            allowed = rule.action == RuleAction::Allow;
        }
    }
    allowed
}

/// Normalize loader-metadata library dialects (fabric meta's `url`+`maven`,
/// bare maven coordinates, side-only `downloads`) into concrete
/// `downloads.artifact` entries. Mirrors the server's `loader.rs` handling
/// so both sides store identical maven-style paths.
fn normalize_loader_libraries(libs: &mut [Library], raw: Option<&[Value]>) {
    let Some(raws) = raw else { return };
    for (lib, value) in libs.iter_mut().zip(raws.iter()) {
        let has_artifact = lib
            .downloads
            .as_ref()
            .and_then(|d| d.artifact.as_ref())
            .is_some();
        if has_artifact {
            continue;
        }
        let mut candidate: Option<(String, String)> = None;
        // launchermeta with only side-specific download objects.
        for side in ["common", "client", "server", "artifact"] {
            if let Some(info) = side_download(value, side) {
                candidate = Some(info);
                break;
            }
        }
        // fabric-meta style: repository base + (`maven` path | coordinates).
        if candidate.is_none() {
            if let Some(repo) = value
                .get("url")
                .and_then(Value::as_str)
                .filter(|u| !u.is_empty())
            {
                let rel = value
                    .get("maven")
                    .and_then(Value::as_str)
                    .filter(|m| !m.is_empty())
                    .map(str::to_string)
                    .or_else(|| coord_path(&lib.name).map(|p| p.to_string_lossy().into_owned()));
                if let Some(rel) = rel {
                    candidate = Some((
                        format!(
                            "{}/{}",
                            repo.trim_end_matches('/'),
                            rel.trim_start_matches('/')
                        ),
                        rel,
                    ));
                }
            }
        }
        if let Some((url, rel)) = candidate {
            lib.downloads = Some(LibraryDownloads {
                artifact: Some(DownloadInfo {
                    path: Some(rel),
                    sha1: None,
                    size: None,
                    url,
                }),
                classifiers: None,
            });
        }
    }
}

/// Pull a `downloads.<side>.url` out of raw json, with the URL-derived
/// local path (host-mirror layout, same as the server's loader store).
fn side_download(value: &Value, side: &str) -> Option<(String, String)> {
    let url = value
        .pointer(&format!("/downloads/{side}/url"))
        .and_then(Value::as_str)
        .filter(|u| !u.is_empty())?
        .to_string();
    let rel = maven_rel(&url)?;
    Some((url, rel.to_string_lossy().into_owned()))
}

/// One filesystem segment: readable chars only, and never just dots.
fn safe_seg(s: &str) -> bool {
    !s.is_empty()
        && !s.bytes().all(|b| b == b'.')
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'+' | b'-'))
}

/// Maven coordinates `group:artifact:version[:classifier][@ext]` → repo path.
pub fn coord_path(coord: &str) -> Option<PathBuf> {
    let (coord, ext) = match coord.split_once('@') {
        Some((c, e)) if safe_seg(e) => (c, e),
        _ => (coord, "jar"),
    };
    let parts: Vec<&str> = coord.split(':').collect();
    if parts.len() < 3 || !parts[..3].iter().all(|p| !p.is_empty()) {
        return None;
    }
    if parts[0].split('.').any(|seg| !safe_seg(seg)) {
        return None;
    }
    if !safe_seg(parts[1]) || !safe_seg(parts[2]) {
        return None;
    }
    let classifier = match parts.get(3) {
        Some(c) if safe_seg(c) => format!("-{c}"),
        Some(_) => return None,
        None => String::new(),
    };
    let mut out = PathBuf::from(parts[0].replace('.', "/"));
    out.push(parts[1]);
    out.push(parts[2]);
    out.push(format!("{}-{}{}.{}", parts[1], parts[2], classifier, ext));
    Some(out)
}

/// URL host+path as a repository-relative path (mirror layout); refuses
/// traversal-ish URLs.
pub fn maven_rel(url: &str) -> Option<PathBuf> {
    let rest = url.split_once("://")?.1;
    let (host, path) = rest.split_once('/')?;
    if host.is_empty()
        || !host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b':' | b'[' | b']'))
        || !host.bytes().any(|b| b.is_ascii_alphanumeric())
    {
        return None;
    }
    let path = path.split('?').next().unwrap_or(path);
    let mut out = PathBuf::from(host);
    for comp in Path::new(path).components() {
        match comp {
            Component::Normal(part) => out.push(part),
            Component::CurDir => {}
            _ => return None,
        }
    }
    Some(out)
}

/// Unpack the natives-classifier jars for this platform into `dest`,
/// skipping `META-INF` and refusing zip-slip entry names.
pub fn extract_natives(
    libraries: &[Library],
    platform: &PlatformInfo,
    libraries_dir: &Path,
    dest: &Path,
) -> Result<usize, RuntimeError> {
    std::fs::create_dir_all(dest)?;
    let mut extracted = 0usize;
    for lib in libraries {
        if !lib.is_applicable(platform) {
            continue;
        }
        let Some(classifier) = lib.natives_classifier(platform) else {
            continue;
        };
        let Some(entry) = lib
            .downloads
            .as_ref()
            .and_then(|d| d.classifiers.as_ref())
            .and_then(|c| c.get(&classifier))
        else {
            continue;
        };
        let Some(rel) = &entry.path else { continue };
        let jar = libraries_dir.join(rel);
        if !jar.is_file() {
            continue;
        }
        extracted += unzip_natives(&jar, dest)?;
    }
    Ok(extracted)
}

fn unzip_natives(jar: &Path, dest: &Path) -> Result<usize, RuntimeError> {
    let file = std::fs::File::open(jar)?;
    let mut archive = zip::ZipArchive::new(file)?;
    let mut count = 0usize;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        let name = entry.name().to_string();
        if name.starts_with("META-INF") || entry.is_dir() {
            continue;
        }
        let out = safe_join(dest, &name)?;
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut data = Vec::new();
        entry.read_to_end(&mut data)?;
        std::fs::write(&out, &data)?;
        #[cfg(unix)]
        if name.ends_with(".so") || name.ends_with(".dylib") {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&out, std::fs::Permissions::from_mode(0o755))?;
        }
        count += 1;
    }
    Ok(count)
}

/// Join a zip entry name under `base`, refusing anything that escapes.
fn safe_join(base: &Path, name: &str) -> Result<PathBuf, RuntimeError> {
    let mut out = base.to_path_buf();
    for comp in Path::new(name).components() {
        match comp {
            Component::Normal(part) => out.push(part),
            Component::CurDir => {}
            _ => {
                return Err(RuntimeError::ManifestError(format!(
                    "unsafe zip entry path: {name}"
                )))
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_ids_are_fenced() {
        assert!(safe_version_id("1.21.1"));
        assert!(safe_version_id("1.20.1-fabric-0.15.11"));
        assert!(!safe_version_id(""));
        assert!(!safe_version_id(".."));
        assert!(!safe_version_id("../evil"));
        assert!(!safe_version_id("a/b"));
        assert!(!safe_version_id("a\\b"));
    }

    #[test]
    fn placeholders_substitute() {
        let vars = HashMap::from([
            ("auth_player_name".to_string(), "Steve".into()),
            ("game_directory".to_string(), "/games/i1/minecraft".into()),
        ]);
        assert_eq!(
            substitute_args(
                "--username ${auth_player_name} --gameDir ${game_directory}",
                &vars
            ),
            "--username Steve --gameDir /games/i1/minecraft"
        );
        // Unknown placeholders empty out; a lone "${" stays as typed.
        assert_eq!(substitute_args("--x ${nope}y ${", &vars), "--x y ${");
    }

    #[test]
    fn coord_paths_match_the_server_loader_module() {
        assert_eq!(
            coord_path("net.fabricmc:fabric-loader:0.14.13"),
            Some(PathBuf::from(
                "net/fabricmc/fabric-loader/0.14.13/fabric-loader-0.14.13.jar"
            ))
        );
        assert_eq!(
            coord_path("g.a:art:1.0:sources@zip"),
            Some(PathBuf::from("g/a/art/1.0/art-1.0-sources.zip"))
        );
        assert!(coord_path("g.a:art:../../x").is_none());
        assert!(coord_path("g.a:art").is_none());
    }

    #[test]
    fn maven_rel_maps_urls_and_refuses_traversal() {
        assert_eq!(
            maven_rel("https://maven.fabricmc.net/net/fabricmc/x/1/x-1.jar"),
            Some(PathBuf::from("maven.fabricmc.net/net/fabricmc/x/1/x-1.jar"))
        );
        assert!(maven_rel("https://host/../x.jar").is_none());
        assert!(maven_rel("no-scheme/x.jar").is_none());
    }

    #[test]
    fn zip_join_refuses_escapes() {
        let base = Path::new("/dest");
        assert_eq!(
            safe_join(base, "a/b.so").unwrap(),
            PathBuf::from("/dest/a/b.so")
        );
        assert!(safe_join(base, "../escape.so").is_err());
        assert!(safe_join(base, "/abs.so").is_err());
    }
}
