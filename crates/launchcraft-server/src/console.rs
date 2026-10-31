//! Admin console: embedded single-page UI plus its JSON endpoints.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Html;
use axum::Json;
use serde::Deserialize;

use crate::{ApiError, AppState};

const INDEX_HTML: &str = include_str!("../console/index.html");

/// The console page itself (static; data still requires the token).
pub async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

pub(crate) fn auth(state: &AppState, headers: &HeaderMap) -> Result<(), ApiError> {
    if state.is_authorized(headers) {
        Ok(())
    } else {
        Err(ApiError::unauthorized())
    }
}

#[derive(Deserialize)]
pub struct PolicyBody {
    pub server_backups_enabled: bool,
}

/// `GET /console/policy`
pub async fn get_policy(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    auth(&state, &headers)?;
    Ok(Json(serde_json::json!({
        "server_backups_enabled": state.store.policy_enabled(),
    })))
}

/// `POST /console/policy` — flips the toggle clients must respect.
pub async fn set_policy(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<PolicyBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    auth(&state, &headers)?;
    state
        .store
        .set_policy_enabled(body.server_backups_enabled)?;
    tracing::info!("upload policy set to {}", body.server_backups_enabled);
    Ok(Json(serde_json::json!({
        "server_backups_enabled": body.server_backups_enabled,
    })))
}

/// `GET /console/stats` — totals for the dashboard view.
pub async fn stats(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    auth(&state, &headers)?;
    let backups = state.store.list()?;
    let total_bytes: u64 = backups.iter().map(|b| b.size_bytes).sum();
    let mut worlds: std::collections::BTreeMap<String, usize> = Default::default();
    for b in &backups {
        *worlds.entry(b.world_name.clone()).or_default() += 1;
    }
    Ok(Json(serde_json::json!({
        "archive_count": backups.len(),
        "total_bytes": total_bytes,
        "newest": backups.first().map(|b| b.file_name.clone()),
        "worlds": worlds
            .into_iter()
            .map(|(world, count)| serde_json::json!({ "world": world, "count": count }))
            .collect::<Vec<_>>(),
    })))
}

/// `GET /console/packs` — registry rows enriched with live dist status.
pub async fn packs_list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    auth(&state, &headers)?;
    let registry = crate::packs::PackRegistry::load(&state.data_dir)?;
    let rows: Vec<serde_json::Value> = registry
        .packs
        .iter()
        .map(|p| {
            let mut row = serde_json::json!({
                "id": p.id,
                "title": p.title,
                "src": p.src.as_ref().map(|s| s.display().to_string()),
                "dist": p.dist.display().to_string(),
                "created_at": p.created_at,
                "curseforge": p.curseforge,
            });
            let status = dist_status(&p.dist);
            if let Some(map) = row.as_object_mut() {
                for (k, v) in status.as_object().unwrap() {
                    map.insert(k.clone(), v.clone());
                }
            }
            row
        })
        .collect();
    Ok(Json(serde_json::json!({ "packs": rows })))
}

/// Reads a pack's `dist/manifest.json` to compute its table status.
pub(crate) fn dist_status(dist: &std::path::Path) -> serde_json::Value {
    let manifest_path = dist.join("manifest.json");
    match std::fs::read_to_string(&manifest_path) {
        Err(_) => serde_json::json!({ "status": "not-built" }),
        Ok(text) => match serde_json::from_str::<launchcraft_core::manifest::PackManifest>(&text) {
            Err(e) => serde_json::json!({
                "status": "error",
                "error": format!("could not parse manifest.json: {e}"),
            }),
            Ok(m) => serde_json::json!({
                "status": "ok",
                "pack_version": m.pack_version,
                "game_version": m.game_version,
                "loader": m.loader,
                "file_count": m.files.len(),
                "total_bytes": m.files.values().map(|e| e.size).sum::<u64>(),
                "created_at": m.created_at.to_rfc3339(),
            }),
        },
    }
}

#[derive(Deserialize)]
pub struct NewPackBody {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub game_version: Option<String>,
}

/// `POST /console/packs/new` — scaffold an editable pack workspace
/// (SKCraft's "New Pack"): source folders + `pack.json` inside the data dir.
pub async fn packs_new(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<NewPackBody>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    auth(&state, &headers)?;
    crate::packs::validate_pack_id(&body.id).map_err(ApiError::from)?;
    let root = state.data_dir.join("packs").join(&body.id);
    let src = root.join("src");
    let dist = root.join("dist");
    for sub in ["mods", "config", "resourcepacks"] {
        std::fs::create_dir_all(src.join(sub))
            .map_err(|e| ApiError::bad_request(format!("could not create {sub}: {e}")))?;
    }
    std::fs::create_dir_all(&dist).map_err(ApiError::from)?;

    let mut config = launchcraft_builder::BuildConfig::load_or_default(None);
    config.pack_id = body.id.clone();
    config.title = body.title.clone();
    if let Some(gv) = body.game_version.as_ref().filter(|g| !g.is_empty()) {
        config.game_version = gv.clone();
    }
    std::fs::write(src.join("pack.json"), serde_json::to_vec_pretty(&config)?)?;

    let mut registry = crate::packs::PackRegistry::load(&state.data_dir)?;
    registry.add(crate::packs::PackEntry {
        id: body.id.clone(),
        title: body.title,
        src: Some(src.clone()),
        dist: dist.clone(),
        created_at: chrono::Utc::now().to_rfc3339(),
        curseforge: None,
    })?;
    registry.save(&state.data_dir)?;
    tracing::info!("created new pack '{}' at {}", body.id, src.display());
    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({
            "id": body.id, "src": src.display().to_string(),
            "dist": dist.display().to_string(), "status": "not-built",
        })),
    ))
}

#[derive(Deserialize)]
pub struct AddPackBody {
    pub id: String,
    pub title: String,
    pub dist: String,
    #[serde(default)]
    pub src: Option<String>,
}

/// `POST /console/packs` — register an existing built directory
/// (SKCraft's "Add Existing").
pub async fn packs_add(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<AddPackBody>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    auth(&state, &headers)?;
    crate::packs::validate_pack_id(&body.id).map_err(ApiError::from)?;
    let dist = std::path::PathBuf::from(&body.dist);
    if !dist.is_dir() {
        return Err(ApiError::bad_request(format!(
            "dist directory does not exist on the server: {}",
            dist.display()
        )));
    }
    let src = body
        .src
        .filter(|s| !s.is_empty())
        .map(std::path::PathBuf::from);
    if let Some(s) = &src {
        if !s.is_dir() {
            return Err(ApiError::bad_request(format!(
                "src directory does not exist on the server: {}",
                s.display()
            )));
        }
    }

    let mut registry = crate::packs::PackRegistry::load(&state.data_dir)?;
    registry.add(crate::packs::PackEntry {
        id: body.id.clone(),
        title: body.title,
        src,
        dist,
        created_at: chrono::Utc::now().to_rfc3339(),
        curseforge: None,
    })?;
    registry.save(&state.data_dir)?;
    tracing::info!("registered pack '{}'", body.id);
    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({ "id": body.id, "added": true })),
    ))
}

#[derive(Deserialize)]
pub struct ModifyPackBody {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub src: Option<String>,
    #[serde(default)]
    pub dist: Option<String>,
}

/// `PUT /console/packs/:id` — SKCraft's "Modify": rename / retarget paths.
pub async fn packs_modify(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<ModifyPackBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    auth(&state, &headers)?;
    let mut registry = crate::packs::PackRegistry::load(&state.data_dir)?;
    let idx = registry
        .packs
        .iter()
        .position(|p| p.id == id)
        .ok_or_else(|| {
            ApiError::new(StatusCode::NOT_FOUND, format!("pack not registered: {id}"))
        })?;
    if let Some(title) = body.title.filter(|t| !t.trim().is_empty()) {
        registry.packs[idx].title = title;
    }
    if let Some(src) = body.src.filter(|s| !s.is_empty()) {
        registry.packs[idx].src = Some(std::path::PathBuf::from(src));
    }
    if let Some(dist) = body.dist.filter(|s| !s.is_empty()) {
        registry.packs[idx].dist = std::path::PathBuf::from(dist);
    }
    registry.save(&state.data_dir)?;
    let entry = &registry.packs[idx];
    let mut row = serde_json::json!({ "id": entry.id, "title": entry.title,
        "src": entry.src.as_ref().map(|s| s.display().to_string()),
        "dist": entry.dist.display().to_string() });
    if let Some(map) = row.as_object_mut() {
        for (k, v) in dist_status(&entry.dist).as_object().unwrap() {
            map.insert(k.clone(), v.clone());
        }
    }
    Ok(Json(row))
}

/// `DELETE /console/packs/:id` — unregister only; files are never deleted.
pub async fn packs_remove(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    auth(&state, &headers)?;
    let mut registry = crate::packs::PackRegistry::load(&state.data_dir)?;
    registry.remove(&id)?;
    registry.save(&state.data_dir)?;
    tracing::info!("unregistered pack '{id}' (files on disk untouched)");
    Ok(Json(serde_json::json!({
        "removed": id, "note": "removed from the registry; files on disk were not deleted",
    })))
}

/// `POST /console/packs/:id/check` — SKCraft's "Check": parse the manifest,
/// confirm every CAS blob is stored, and re-hash it (zstd-decompress → BLAKE3).
pub async fn packs_check(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    auth(&state, &headers)?;
    let registry = crate::packs::PackRegistry::load(&state.data_dir)?;
    let dist = registry
        .find(&id)
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, format!("pack not registered: {id}")))?
        .dist
        .clone();
    let report = tokio::task::spawn_blocking(move || verify_pack_dist(&dist))
        .await
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    Ok(Json(report))
}

fn verify_pack_dist(dist: &std::path::Path) -> serde_json::Value {
    let manifest: launchcraft_core::manifest::PackManifest =
        match std::fs::read_to_string(dist.join("manifest.json"))
            .map_err(|e| format!("could not read manifest.json: {e}"))
            .and_then(|t| {
                serde_json::from_str(&t).map_err(|e| format!("could not parse manifest.json: {e}"))
            }) {
            Ok(m) => m,
            Err(error) => return serde_json::json!({ "ok": false, "error": error }),
        };

    let mut missing = 0usize;
    let mut hash_mismatch = 0usize;
    let mut problems: Vec<String> = Vec::new();
    for (path, entry) in &manifest.files {
        let shard = &entry.hash[..2.min(entry.hash.len())];
        let blob_path = dist
            .join("cas")
            .join("objects")
            .join(shard)
            .join(&entry.hash);
        let blob = match std::fs::read(&blob_path) {
            Ok(b) => b,
            Err(_) => {
                missing += 1;
                if problems.len() < 10 {
                    problems.push(format!(
                        "{path}: CAS blob not found ({shard}/{})",
                        &entry.hash[..12.min(entry.hash.len())]
                    ));
                }
                continue;
            }
        };
        let content = match zstd::decode_all(std::io::Cursor::new(&blob)) {
            Ok(c) => c,
            Err(e) => {
                hash_mismatch += 1;
                if problems.len() < 10 {
                    problems.push(format!("{path}: blob is not a readable zstd frame: {e}"));
                }
                continue;
            }
        };
        let actual = blake3::hash(&content).to_hex().to_string();
        if actual != entry.hash {
            hash_mismatch += 1;
            if problems.len() < 10 {
                problems.push(format!(
                    "{path}: hash mismatch (expected {}, got {actual})",
                    &entry.hash[..12.min(entry.hash.len())]
                ));
            }
        }
    }
    serde_json::json!({
        "ok": missing == 0 && hash_mismatch == 0,
        "files": manifest.files.len(),
        "missing": missing,
        "hash_mismatch": hash_mismatch,
        "problems": problems,
    })
}

#[derive(Deserialize)]
pub struct BuildPackBody {
    #[serde(default)]
    pub version: Option<String>,
}

/// `POST /console/packs/:id/build` — SKCraft's "Build": run the pack builder
/// over the pack's source tree into its dist directory.
pub async fn packs_build(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, ApiError> {
    auth(&state, &headers)?;
    let version = (body.get("version").and_then(|v| v.as_str()))
        .filter(|v| !v.is_empty())
        .map(|v| v.to_string());
    let registry = crate::packs::PackRegistry::load(&state.data_dir)?;
    let entry = registry
        .find(&id)
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, format!("pack not registered: {id}")))?
        .clone();
    let src = entry.src.clone().ok_or_else(|| {
        ApiError::bad_request("pack has no source tree — set one via Modify, or use New Pack")
    })?;

    let built = run_pack_build(src, entry.dist, entry.id, version).await?;
    tracing::info!("built pack '{id}'");
    Ok(Json(built))
}

/// Run the pack builder over a workspace. Shared by the Build endpoint and
/// the CurseForge import/update handlers.
pub(crate) async fn run_pack_build(
    src: std::path::PathBuf,
    dist: std::path::PathBuf,
    pack_id: String,
    version: Option<String>,
) -> Result<serde_json::Value, ApiError> {
    tokio::task::spawn_blocking(move || {
        let mut config = launchcraft_builder::BuildConfig::load_or_default(Some(
            src.join("pack.json").as_path(),
        ));
        config.pack_id = pack_id.clone();
        if let Some(v) = version {
            config.version = v;
        }
        launchcraft_builder::PackBuilder::new(&src, &dist, config)
            .build()
            .map(|manifest| {
                serde_json::json!({
                    "built": true, "id": pack_id, "pack_version": manifest.pack_version,
                    "file_count": manifest.files.len(),
                    "total_bytes": manifest.files.values().map(|e| e.size).sum::<u64>(),
                })
            })
            .map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
    .map_err(|e| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("build failed: {e}"),
        )
    })
}
