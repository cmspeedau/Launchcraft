//! Unified modpack browser + importer for the console: CurseForge,
//! Modrinth and FTB behind one set of endpoints.
//!
//! The console talks only to *this* server; provider credentials (the
//! CurseForge `x-api-key`) never reach a browser. Providers share one
//! import pipeline: download the pack archive, read its manifest —
//! `manifest.json` (Forge/CurseForge style), `modrinth.index.json`
//! (Modrinth/`mrpack`) or a baked `minecraft/` tree (legacy FTB) — fetch
//! the referenced files into the pack's `src/`, merge overrides, register
//! the workspace and build it. Provenance is stored per pack so "Update"
//! can re-import a newer version in place.

use std::path::Path;
use std::sync::Arc;

use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{ApiError, AppState};

/// Which modpack host a request targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Curseforge,
    Modrinth,
    Ftb,
}

impl Kind {
    pub(crate) fn parse(raw: &str) -> Result<Self, ApiError> {
        match raw {
            "curseforge" => Ok(Kind::Curseforge),
            "modrinth" => Ok(Kind::Modrinth),
            "ftb" => Ok(Kind::Ftb),
            other => Err(ApiError::bad_request(format!(
                "unknown modpack provider '{other}' (use curseforge, modrinth or ftb)"
            ))),
        }
    }
    pub(crate) fn id(self) -> &'static str {
        match self {
            Kind::Curseforge => "curseforge",
            Kind::Modrinth => "modrinth",
            Kind::Ftb => "ftb",
        }
    }
    pub(crate) fn label(self) -> &'static str {
        match self {
            Kind::Curseforge => "CurseForge",
            Kind::Modrinth => "Modrinth",
            Kind::Ftb => "FTB",
        }
    }
    fn configured(self, state: &AppState) -> bool {
        match self {
            Kind::Curseforge => state.curse_key.is_some(),
            // Modrinth and FTB are open APIs; nothing to configure.
            Kind::Modrinth | Kind::Ftb => true,
        }
    }
    fn require(self, state: &AppState) -> Result<(), ApiError> {
        if self.configured(state) {
            return Ok(());
        }
        Err(ApiError::bad_request(format!(
            "{} is not configured on this server: set LAUNCHCRAFT_CURSEFORGE_KEY \
             (free key from https://developers.curseforge.org) and restart",
            self.label()
        )))
    }

    async fn search(
        self,
        state: &Arc<AppState>,
        args: &SearchArgs<'_>,
    ) -> Result<(Vec<Found>, u64), ApiError> {
        match self {
            Kind::Curseforge => crate::curseforge::search(state, args).await,
            Kind::Modrinth => crate::modrinth::search(state, args).await,
            Kind::Ftb => crate::ftb::search(state, args).await,
        }
    }
    async fn versions(
        self,
        state: &Arc<AppState>,
        project: &str,
    ) -> Result<Vec<PackVersion>, ApiError> {
        match self {
            Kind::Curseforge => crate::curseforge::versions(state, project).await,
            Kind::Modrinth => crate::modrinth::versions(state, project).await,
            Kind::Ftb => crate::ftb::versions(state, project).await,
        }
    }
    async fn latest(self, state: &Arc<AppState>, project: &str) -> Result<String, ApiError> {
        match self {
            Kind::Curseforge => crate::curseforge::latest(state, project).await,
            Kind::Modrinth => crate::modrinth::latest(state, project).await,
            Kind::Ftb => crate::ftb::latest(state, project).await,
        }
    }
    async fn fetch(
        self,
        state: &Arc<AppState>,
        project: &str,
        file: &str,
    ) -> Result<(Vec<u8>, Option<FallbackMeta>), ApiError> {
        match self {
            Kind::Curseforge => crate::curseforge::download(state, project, file).await,
            Kind::Modrinth => crate::modrinth::download(state, project, file).await,
            Kind::Ftb => crate::ftb::download(state, project, file).await,
        }
    }
}

/// A modpack as listed by any provider.
pub(crate) struct Found {
    pub id: String,
    pub name: String,
    pub description: String,
    pub icon: String,
    pub downloads: u64,
}

/// One downloadable version of a modpack.
pub(crate) struct PackVersion {
    pub id: String,
    pub name: String,
    pub date: String,
    pub size: u64,
    pub game_versions: Vec<String>,
}

/// Normalised search parameters shared by all providers.
pub(crate) struct SearchArgs<'a> {
    pub q: Option<&'a str>,
    /// featured | popularity | lastupdated | name
    pub sort: &'a str,
    pub game_version: Option<&'a str>,
    /// Zero-based result page (providers page at `PAGE_SIZE`).
    pub index: u32,
}

/// Name/version hints for archives that carry no manifest (legacy FTB).
#[derive(Debug, Clone)]
pub(crate) struct FallbackMeta {
    pub name: String,
    pub version: String,
    pub game_version: String,
}

// ---------------------------------------------------------------- handlers

/// `GET /console/modpacks` — which providers this server can browse.
pub async fn providers(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    crate::console::auth(&state, &headers)?;
    let all = [Kind::Curseforge, Kind::Modrinth, Kind::Ftb];
    Ok(Json(json!({
        "providers": all.iter().map(|k| json!({
            "id": k.id(),
            "name": k.label(),
            "configured": k.configured(&state),
            "hint": (!k.configured(&state)).then_some(
                "Set LAUNCHCRAFT_CURSEFORGE_KEY (a free key from \
                 https://developers.curseforge.org) to enable browsing.",
            ),
        })).collect::<Vec<_>>(),
    })))
}

#[derive(Debug, Deserialize)]
pub struct SearchQuery {
    pub provider: String,
    #[serde(default)]
    pub q: Option<String>,
    /// featured | popularity | lastupdated | name
    #[serde(default)]
    pub sort: Option<String>,
    #[serde(default)]
    pub game_version: Option<String>,
    #[serde(default)]
    pub index: Option<u32>,
}

/// `GET /console/modpacks/search` — trim any provider's results to the
/// shape the console list needs.
pub async fn search(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<SearchQuery>,
) -> Result<Json<Value>, ApiError> {
    crate::console::auth(&state, &headers)?;
    let kind = Kind::parse(&q.provider)?;
    kind.require(&state)?;
    let args = SearchArgs {
        q: q.q.as_deref().map(str::trim).filter(|t| !t.is_empty()),
        sort: q.sort.as_deref().unwrap_or("featured"),
        game_version: q
            .game_version
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty()),
        index: q.index.unwrap_or(0),
    };
    let (packs, total) = kind.search(&state, &args).await?;
    let packs: Vec<Value> = packs
        .into_iter()
        .map(|p| {
            json!({
                "id": p.id, "name": p.name, "description": p.description,
                "icon": p.icon, "downloads": p.downloads,
            })
        })
        .collect();
    Ok(Json(json!({ "packs": packs, "total": total })))
}

#[derive(Debug, Deserialize)]
pub struct VersionsQuery {
    pub provider: String,
    pub project: String,
}

/// `GET /console/modpacks/versions?provider=&project=` — newest first.
pub async fn versions(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<VersionsQuery>,
) -> Result<Json<Value>, ApiError> {
    crate::console::auth(&state, &headers)?;
    let kind = Kind::parse(&q.provider)?;
    kind.require(&state)?;
    let versions = kind.versions(&state, &q.project).await?;
    let versions: Vec<Value> = versions
        .into_iter()
        .map(|v| {
            json!({
                "id": v.id, "name": v.name, "date": v.date,
                "size": v.size, "game_versions": v.game_versions,
            })
        })
        .collect();
    Ok(Json(json!({ "versions": versions })))
}

#[derive(Debug, Deserialize)]
pub struct AddBody {
    pub provider: String,
    /// Provider project handle (project id / slug / pack dir).
    pub project: String,
    /// Provider file/version handle.
    pub file: String,
    /// launchcraft pack id for the registry.
    pub id: String,
    #[serde(default)]
    pub title: Option<String>,
}

/// `POST /console/modpacks/add` — download, unpack, fetch files, build.
pub async fn add(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<AddBody>,
) -> Result<Json<Value>, ApiError> {
    crate::console::auth(&state, &headers)?;
    let kind = Kind::parse(&body.provider)?;
    kind.require(&state)?;
    crate::packs::validate_pack_id(&body.id).map_err(ApiError::from)?;
    let mut registry = crate::packs::PackRegistry::load(&state.data_dir)?;
    if registry.find(&body.id).is_some() {
        return Err(ApiError::bad_request(format!(
            "pack id already registered: {}",
            body.id
        )));
    }
    let pack_id = body.id.clone();

    let root = state.data_dir.join("packs").join(&pack_id);
    let src = root.join("src");
    let dist = root.join("dist");
    std::fs::create_dir_all(&dist)?;

    // Download the archive, fetch everything it references into src/.
    let (bytes, meta) = kind.fetch(&state, &body.project, &body.file).await?;
    let outcome = import_pack_zip(&state, bytes, &src, meta).await?;
    let plan = outcome.plan;

    // pack.json + registry + build.
    let title = body
        .title
        .clone()
        .filter(|t| !t.trim().is_empty())
        .unwrap_or_else(|| plan.name.clone());
    let mut config = launchcraft_builder::BuildConfig::load_or_default(None);
    config.pack_id = pack_id.clone();
    config.title = title.clone();
    config.version = if plan.version.trim().is_empty() {
        "1.0".into()
    } else {
        plan.version.clone()
    };
    if !plan.game_version.trim().is_empty() {
        config.game_version = plan.game_version.clone();
    }
    std::fs::write(src.join("pack.json"), serde_json::to_vec_pretty(&config)?)?;

    registry.add(crate::packs::PackEntry {
        id: pack_id.clone(),
        title,
        src: Some(src.clone()),
        dist: dist.clone(),
        created_at: chrono::Utc::now().to_rfc3339(),
        source: Some(crate::packs::PackSource {
            provider: kind.id().to_string(),
            project: body.project.clone(),
            file: body.file.clone(),
        }),
    })?;
    registry.save(&state.data_dir)?;

    let built = crate::console::run_pack_build(src, dist, pack_id.clone(), None).await?;

    tracing::info!(
        "imported {} pack '{pack_id}' (project {} file {}): {} file(s) fetched",
        kind.label(),
        body.project,
        body.file,
        outcome.fetched
    );
    Ok(Json(json!({
        "id": pack_id,
        "provider": kind.id(),
        "built": true,
        "mods_fetched": outcome.fetched,
        "warnings": outcome.warnings,
        "game_version": plan.game_version,
        "loader": plan.loader,
        "pack_version": built["pack_version"],
        "file_count": built["file_count"],
        "total_bytes": built["total_bytes"],
    })))
}

#[derive(Debug, Deserialize)]
pub struct UploadQuery {
    /// launchcraft pack id for the registry.
    pub id: String,
    #[serde(default)]
    pub title: Option<String>,
    /// Baked-tree archives carry no manifest, so name/version metadata
    /// comes from these fields when the zip is a plain `minecraft/` tree.
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub game_version: Option<String>,
}

/// `POST /console/modpacks/upload` — the request body IS the modpack zip
/// (a CurseForge zip, a `.mrpack`, or a baked `minecraft/` tree). Same
/// import pipeline as the providers, minus the download step; the pack
/// simply has no provenance, so there is no Update button for it.
pub async fn upload(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<UploadQuery>,
    body: axum::body::Bytes,
) -> Result<Json<Value>, ApiError> {
    crate::console::auth(&state, &headers)?;
    if body.is_empty() {
        return Err(ApiError::bad_request("upload body is empty"));
    }
    crate::packs::validate_pack_id(&q.id).map_err(ApiError::from)?;
    let mut registry = crate::packs::PackRegistry::load(&state.data_dir)?;
    if registry.find(&q.id).is_some() {
        return Err(ApiError::bad_request(format!(
            "pack id already registered: {}",
            q.id
        )));
    }
    let pack_id = q.id.clone();

    let root = state.data_dir.join("packs").join(&pack_id);
    let src = root.join("src");
    let dist = root.join("dist");
    std::fs::create_dir_all(&dist)?;

    // Only consulted if the archive turns out to be a manifest-less tree.
    let meta = FallbackMeta {
        name: q.title.clone().unwrap_or_else(|| q.id.clone()),
        version: q.version.clone().unwrap_or_else(|| "1.0".into()),
        game_version: q.game_version.clone().unwrap_or_default(),
    };
    let outcome = import_pack_zip(&state, body.to_vec(), &src, Some(meta)).await?;
    let plan = outcome.plan;

    let title = q
        .title
        .clone()
        .filter(|t| !t.trim().is_empty())
        .unwrap_or_else(|| plan.name.clone());
    let mut config = launchcraft_builder::BuildConfig::load_or_default(None);
    config.pack_id = pack_id.clone();
    config.title = title.clone();
    config.version = if plan.version.trim().is_empty() {
        "1.0".into()
    } else {
        plan.version.clone()
    };
    if !plan.game_version.trim().is_empty() {
        config.game_version = plan.game_version.clone();
    }
    std::fs::write(src.join("pack.json"), serde_json::to_vec_pretty(&config)?)?;

    registry.add(crate::packs::PackEntry {
        id: pack_id.clone(),
        title,
        src: Some(src.clone()),
        dist: dist.clone(),
        created_at: chrono::Utc::now().to_rfc3339(),
        // A hand-supplied archive has no upstream to update from.
        source: None,
    })?;
    registry.save(&state.data_dir)?;

    let built = crate::console::run_pack_build(src, dist, pack_id.clone(), None).await?;

    tracing::info!(
        "imported uploaded pack '{pack_id}': {} file(s) in the tree",
        outcome.fetched
    );
    Ok(Json(json!({
        "id": pack_id,
        "provider": "local",
        "built": true,
        "mods_fetched": outcome.fetched,
        "warnings": outcome.warnings,
        "game_version": plan.game_version,
        "loader": plan.loader,
        "pack_version": built["pack_version"],
        "file_count": built["file_count"],
        "total_bytes": built["total_bytes"],
    })))
}

#[derive(Debug, Deserialize)]
pub struct UpdateBody {
    /// Explicit target version; defaults to the project's newest.
    #[serde(default)]
    pub file: Option<String>,
}

/// `POST /console/modpacks/update/:id` — re-import a sourced pack at a
/// newer provider file and rebuild it. The pack id and registry title are
/// kept; the mod list is rebuilt (stale mods pruned) and overrides
/// re-merged. Body: optionally `{ "file": "..." }`.
pub async fn update(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    UrlPath(id): UrlPath<String>,
    body: Option<Json<UpdateBody>>,
) -> Result<Json<Value>, ApiError> {
    crate::console::auth(&state, &headers)?;
    let mut registry = crate::packs::PackRegistry::load(&state.data_dir)?;
    let idx = registry
        .packs
        .iter()
        .position(|p| p.id == id)
        .ok_or_else(|| {
            ApiError::new(StatusCode::NOT_FOUND, format!("pack not registered: {id}"))
        })?;
    let entry = registry.packs[idx].clone();
    let source = entry.source.clone().ok_or_else(|| {
        ApiError::bad_request(format!(
            "pack '{id}' was not imported from a modpack host, so there is nothing to update"
        ))
    })?;
    let kind = Kind::parse(&source.provider)?;
    kind.require(&state)?;
    let src = entry.src.clone().ok_or_else(|| {
        ApiError::bad_request(format!("pack '{id}' has no source tree to update"))
    })?;
    if !src.is_dir() {
        return Err(ApiError::bad_request(format!(
            "pack '{id}' source tree is missing: {}",
            src.display()
        )));
    }

    // Which file to move to: the explicit pick, else the project's newest.
    let requested = body.and_then(|Json(b)| b.file).filter(|f| !f.is_empty());
    let target = match requested {
        Some(f) => f,
        None => kind.latest(&state, &source.project).await?,
    };
    if target == source.file {
        return Ok(Json(json!({
            "id": id, "provider": kind.id(), "updated": false, "file": target,
            "note": "the pack already tracks this version",
        })));
    }

    // Legacy FTB packs are whole baked trees: replace the workspace
    // outright. Manifest-based packs are re-imported in place and pruned
    // afterwards, so a failed import breaks nothing.
    if kind == Kind::Ftb {
        std::fs::remove_dir_all(&src)?;
    }

    let (bytes, meta) = kind.fetch(&state, &source.project, &target).await?;
    let outcome = import_pack_zip(&state, bytes, &src, meta).await?;
    let pruned = if outcome.plan.baked {
        0
    } else {
        prune_mods(&src, &outcome.written)?
    };

    // pack.json: keep id + registry title, adopt the new version.
    let pack_json = src.join("pack.json");
    let mut config = launchcraft_builder::BuildConfig::load_or_default(
        pack_json.is_file().then_some(pack_json.as_path()),
    );
    config.pack_id = id.clone();
    config.title = entry.title.clone();
    if !outcome.plan.version.trim().is_empty() {
        config.version = outcome.plan.version.clone();
    }
    if !outcome.plan.game_version.trim().is_empty() {
        config.game_version = outcome.plan.game_version.clone();
    }
    std::fs::write(&pack_json, serde_json::to_vec_pretty(&config)?)?;
    registry.packs[idx].source = Some(crate::packs::PackSource {
        provider: source.provider.clone(),
        project: source.project.clone(),
        file: target.clone(),
    });
    registry.save(&state.data_dir)?;

    let built = crate::console::run_pack_build(src, entry.dist, id.clone(), None).await?;
    tracing::info!(
        "updated {} pack '{id}' (project {}) {} -> {}",
        kind.label(),
        source.project,
        source.file,
        target
    );
    Ok(Json(json!({
        "id": id, "provider": kind.id(), "updated": true,
        "from_file": source.file, "file": target,
        "mods_fetched": outcome.fetched, "pruned": pruned, "warnings": outcome.warnings,
        "pack_version": built["pack_version"],
        "file_count": built["file_count"],
        "total_bytes": built["total_bytes"],
    })))
}

// ----------------------------------------------------------------- import

/// Download a file body (mod jar / modpack archive) with a hard size cap.
pub(crate) async fn fetch_bytes(state: &AppState, url: &str) -> Result<Vec<u8>, ApiError> {
    let mut res = state
        .http
        .get(url)
        .send()
        .await
        .map_err(|e| ApiError::new(StatusCode::BAD_GATEWAY, format!("download failed: {e}")))?;
    if !res.status().is_success() {
        return Err(ApiError::new(
            StatusCode::BAD_GATEWAY,
            format!("download failed ({}): {url}", res.status()),
        ));
    }
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = res
        .chunk()
        .await
        .map_err(|e| ApiError::new(StatusCode::BAD_GATEWAY, format!("download failed: {e}")))?
    {
        if buf.len() + chunk.len() > state.max_upload_bytes {
            return Err(ApiError::bad_request(format!(
                "download exceeds the {} MB size cap: {url}",
                state.max_upload_bytes / (1024 * 1024)
            )));
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

/// What one successful import produced.
pub(crate) struct ImportOutcome {
    pub plan: ImportPlan,
    /// Files that landed in the tree (mods, or every baked file).
    pub fetched: usize,
    /// Optional mods that could not be downloaded, described.
    pub warnings: Vec<String>,
    /// `src`-relative paths this import actually wrote (for update pruning).
    pub written: Vec<String>,
}

/// A referenced mod file inside a pack manifest.
#[derive(Clone, Debug)]
struct ModFile {
    project_id: u64,
    file_id: u64,
    required: bool,
    /// Direct-download packs (`modrinth.index.json`) give a path + URL;
    /// Forge manifests resolve through the CurseForge API instead.
    direct: Option<(String, String)>,
}

pub(crate) struct ImportPlan {
    name: String,
    version: String,
    game_version: String,
    loader: String,
    mods: Vec<ModFile>,
    overrides: std::path::PathBuf,
    /// Legacy FTB: the archive IS the game tree (`minecraft/…`).
    baked: bool,
}

/// Download a pack archive and (re)create its contents inside `src`:
/// fetch every referenced file, then merge `overrides/` onto the tree.
/// The staging directory is removed on every path, errors included.
pub(crate) async fn import_pack_zip(
    state: &Arc<AppState>,
    zip_bytes: Vec<u8>,
    src: &Path,
    meta: Option<FallbackMeta>,
) -> Result<ImportOutcome, ApiError> {
    // Unpack to staging (zip-slip safe), then read the pack manifest.
    let staging = state.data_dir.join("tmp").join(format!(
        "pack-{}",
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
    ));
    std::fs::create_dir_all(&staging)?;
    let plan = (|| -> Result<ImportPlan, ApiError> {
        extract_zip(&zip_bytes, &staging)?;
        read_plan(&staging, meta)
    })();
    let plan = match plan {
        Ok(p) => p,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(e);
        }
    };

    // Baked trees (legacy FTB) are copied wholesale; manifest packs get
    // their referenced files fetched. Either way staging must go away.
    let mut fetched = 0usize;
    let mut written: Vec<String> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let fetch = async {
        std::fs::create_dir_all(src.join("mods"))?;
        std::fs::create_dir_all(src.join("config"))?;
        if plan.baked {
            copy_tree(&staging.join("minecraft"), src, src, &mut written)?;
            return Ok::<(), ApiError>(());
        }
        let mut set = tokio::task::JoinSet::new();
        let mut in_flight = 0usize;
        for mf in plan.mods.clone() {
            let st = state.clone();
            let src = src.to_path_buf();
            set.spawn(async move {
                let result: Result<String, ApiError> = async {
                    let (rel, url) = match &mf.direct {
                        Some((rel, url)) => (rel.clone(), url.clone()),
                        None => {
                            let (rel, url) =
                                crate::curseforge::resolve_mod_file(&st, mf.project_id, mf.file_id)
                                    .await?;
                            (rel, url)
                        }
                    };
                    let bytes = fetch_bytes(&st, &url).await?;
                    let target = src.join(&rel);
                    if let Some(parent) = target.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    std::fs::write(&target, bytes)?;
                    Ok(rel)
                }
                .await;
                (mf, result)
            });
            in_flight += 1;
            // Keep at most 8 concurrent fetches: polite to CDNs, simple code.
            if in_flight == 8 {
                drain_one(
                    &mut set,
                    &mut in_flight,
                    &mut fetched,
                    &mut written,
                    &mut warnings,
                )
                .await?;
            }
        }
        while in_flight > 0 {
            drain_one(
                &mut set,
                &mut in_flight,
                &mut fetched,
                &mut written,
                &mut warnings,
            )
            .await?;
        }
        // Overrides (configs, scripts, ...) merge straight into src/.
        if plan.overrides.is_dir() {
            merge_dir(&plan.overrides, src)?;
        }
        Ok::<(), ApiError>(())
    }
    .await;
    let _ = std::fs::remove_dir_all(&staging);
    fetch?;

    // For baked trees every copied file counts as "fetched"; the warnings
    // list stayed empty because nothing was downloaded.
    let fetched = if plan.baked { written.len() } else { fetched };
    Ok(ImportOutcome {
        plan,
        fetched,
        warnings,
        written,
    })
}

/// Join one spawned mod fetch: required-mod failures abort the import,
/// optional ones become warnings. Successes record the relative path that
/// landed so an update can tell kept files from stale ones.
async fn drain_one(
    set: &mut tokio::task::JoinSet<(ModFile, Result<String, ApiError>)>,
    in_flight: &mut usize,
    fetched: &mut usize,
    written: &mut Vec<String>,
    warnings: &mut Vec<String>,
) -> Result<(), ApiError> {
    let (mf, result) = set
        .join_next()
        .await
        .ok_or_else(|| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "mod task vanished"))?
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    *in_flight -= 1;
    match result {
        Ok(rel) => {
            *fetched += 1;
            written.push(rel);
        }
        Err(e) => {
            if mf.required {
                return Err(e);
            }
            let what = match &mf.direct {
                Some((rel, _)) => rel.clone(),
                None => format!("{}:{}", mf.project_id, mf.file_id),
            };
            tracing::warn!("optional mod file {what} skipped: {}", e.message);
            warnings.push(format!("optional mod {what} skipped: {}", e.message));
        }
    }
    Ok(())
}

/// Delete files under `src/mods` that the fresh import did not write, so
/// mods removed between pack versions do not linger.
fn prune_mods(src: &Path, written: &[String]) -> Result<usize, ApiError> {
    let keep: std::collections::HashSet<String> =
        written.iter().map(|s| s.replace('\\', "/")).collect();
    let mods = src.join("mods");
    if !mods.is_dir() {
        return Ok(0);
    }
    let mut removed = 0usize;
    let mut stack = vec![mods];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let path = entry?.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let rel = path
                .strip_prefix(src)
                .map(|r| r.to_string_lossy().replace('\\', "/"))
                .unwrap_or_default();
            if !keep.contains(&rel) {
                std::fs::remove_file(&path)?;
                removed += 1;
            }
        }
    }
    Ok(removed)
}

/// Read `manifest.json` (Forge/CurseForge), `modrinth.index.json`
/// (mrpack) or a baked `minecraft/` tree (FTB) from an unpacked archive.
fn read_plan(staging: &Path, meta: Option<FallbackMeta>) -> Result<ImportPlan, ApiError> {
    if staging.join("manifest.json").is_file() {
        let text = std::fs::read_to_string(staging.join("manifest.json"))?;
        let m: Value = serde_json::from_str(&text)
            .map_err(|e| ApiError::bad_request(format!("bad modpack manifest.json: {e}")))?;
        let mods = m
            .get("files")
            .and_then(Value::as_array)
            .map(|files| {
                files
                    .iter()
                    .filter_map(|f| {
                        Some(ModFile {
                            project_id: f.get("projectID").and_then(Value::as_u64)?,
                            file_id: f.get("fileID").and_then(Value::as_u64)?,
                            required: f.get("required").and_then(Value::as_bool).unwrap_or(true),
                            direct: None,
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        let overrides_name = m
            .get("overrides")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or("overrides");
        Ok(ImportPlan {
            name: m
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("CursePack")
                .to_string(),
            version: m
                .get("version")
                .and_then(Value::as_str)
                .unwrap_or("1.0")
                .to_string(),
            game_version: m
                .pointer("/minecraft/version")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            loader: m
                .pointer("/minecraft/modLoaders/0/id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            mods,
            overrides: staging.join(overrides_name),
            baked: false,
        })
    } else if staging.join("modrinth.index.json").is_file() {
        let text = std::fs::read_to_string(staging.join("modrinth.index.json"))?;
        let m: Value = serde_json::from_str(&text)
            .map_err(|e| ApiError::bad_request(format!("bad modrinth.index.json: {e}")))?;
        let mods = m
            .get("files")
            .and_then(Value::as_array)
            .map(|files| {
                files
                    .iter()
                    .enumerate()
                    .filter_map(|(i, f)| {
                        let url = f
                            .get("downloads")
                            .and_then(Value::as_array)
                            .and_then(|d| d.first())
                            .and_then(Value::as_str)?
                            .to_string();
                        let rel = f
                            .get("path")
                            .and_then(Value::as_str)
                            .and_then(safe_rel_path)
                            .unwrap_or_else(|| format!("mods/mod-{i}.jar"));
                        Some(ModFile {
                            project_id: 0,
                            file_id: i as u64,
                            required: f
                                .pointer("/env/client")
                                .and_then(Value::as_str)
                                .map(|v| v != "unsupported")
                                .unwrap_or(true),
                            direct: Some((rel, url)),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(ImportPlan {
            name: m
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("FabricPack")
                .to_string(),
            version: m
                .get("versionId")
                .or_else(|| m.get("version"))
                .and_then(Value::as_str)
                .unwrap_or("1.0")
                .to_string(),
            game_version: m
                .get("gameVersions")
                .and_then(Value::as_array)
                .and_then(|g| {
                    g.iter()
                        .filter_map(|v| v.as_str())
                        .find(|v| v.starts_with("1."))
                })
                .unwrap_or_default()
                .to_string(),
            loader: m
                .get("gameVersions")
                .and_then(Value::as_array)
                .and_then(|g| {
                    g.iter().filter_map(|v| v.as_str()).find(|v| {
                        v.contains("fabric") || v.contains("forge") || v.contains("neoforge")
                    })
                })
                .unwrap_or_default()
                .to_string(),
            mods,
            overrides: staging.join("overrides"),
            baked: false,
        })
    } else if staging.join("minecraft").is_dir() {
        // Legacy FTB: a complete game tree, metadata from the catalog.
        let meta = meta.ok_or_else(|| {
            ApiError::bad_request("baked pack archive arrived without catalog metadata")
        })?;
        Ok(ImportPlan {
            name: meta.name,
            version: meta.version,
            game_version: meta.game_version,
            loader: String::new(),
            mods: Vec::new(),
            overrides: staging.join("overrides"),
            baked: true,
        })
    } else {
        Err(ApiError::bad_request(
            "modpack archive has neither manifest.json, modrinth.index.json nor a minecraft/ tree",
        ))
    }
}

/// Unzip into `dest`, refusing entries that try to escape it.
fn extract_zip(bytes: &[u8], dest: &Path) -> Result<(), ApiError> {
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes))
        .map_err(|e| ApiError::bad_request(format!("unreadable modpack zip: {e}")))?;
    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| ApiError::bad_request(format!("bad zip entry: {e}")))?;
        let Some(rel) = entry.enclosed_name() else {
            continue;
        };
        let out = dest.join(&rel);
        if entry.is_dir() {
            std::fs::create_dir_all(out)?;
        } else {
            if let Some(parent) = out.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut file = std::fs::File::create(&out)?;
            std::io::copy(&mut entry, &mut file)?;
        }
    }
    Ok(())
}

/// Recursively copy `from` over `to`, creating directories as needed.
fn merge_dir(from: &Path, to: &Path) -> Result<(), ApiError> {
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let target = to.join(entry.file_name());
        if ty.is_dir() {
            std::fs::create_dir_all(&target)?;
            merge_dir(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

/// Copy `from` into `base`, recording every file's `base`-relative path.
fn copy_tree(
    from: &Path,
    to: &Path,
    base: &Path,
    written: &mut Vec<String>,
) -> Result<(), ApiError> {
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let target = to.join(entry.file_name());
        if ty.is_dir() {
            std::fs::create_dir_all(&target)?;
            copy_tree(&entry.path(), &target, base, written)?;
        } else {
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(entry.path(), &target)?;
            if let Ok(rel) = target.strip_prefix(base) {
                written.push(rel.to_string_lossy().replace('\\', "/"));
            }
        }
    }
    Ok(())
}

/// Keep only the basename of a remote filename, or None if unusable.
pub(crate) fn safe_file_name(raw: &str) -> Option<String> {
    let base = Path::new(raw)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .filter(|n| !n.is_empty() && n != "." && n != "..")?;
    (!base.contains('/') && !base.contains('\\')).then_some(base)
}

/// Accept only plain relative paths under `mods/` / `config/` etc.
fn safe_rel_path(raw: &str) -> Option<String> {
    let p = Path::new(raw);
    let ok = p
        .components()
        .all(|c| matches!(c, std::path::Component::Normal(_)));
    ok.then(|| raw.to_string())
}
