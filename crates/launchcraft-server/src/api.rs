//! World backup API endpoints spoken by `launchcraft-backup::BackupSyncClient`.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use launchcraft_core::instance::BackupArchiveInfo;

use crate::{ApiError, AppState, FilenameQuery};

/// `GET /backups` — stored archives plus the current upload policy.
pub async fn list_backups(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    if !state.is_authorized(&headers) {
        return Err(ApiError::unauthorized());
    }
    let backups = state.store.list()?;
    Ok(Json(serde_json::json!({
        "backups": backups,
        "server_backups_enabled": state.store.policy_enabled(),
    })))
}

/// `POST /backups/upload?filename=<name>.zip` with the raw zip as body.
///
/// Honors the server-side policy: while `server_backups_enabled` is false the
/// store refuses uploads with `403`, mirroring the client-side refusal.
pub async fn upload_backup(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<FilenameQuery>,
    body: Bytes,
) -> Result<(StatusCode, Json<BackupArchiveInfo>), ApiError> {
    if !state.is_authorized(&headers) {
        return Err(ApiError::unauthorized());
    }
    if !state.store.policy_enabled() {
        return Err(ApiError::forbidden(
            "cloud uploads are disabled by server policy",
        ));
    }
    let info = state.store.save(&q.filename, &body)?;
    tracing::info!("stored {} ({} bytes)", info.file_name, info.size_bytes);
    Ok((StatusCode::CREATED, Json(info)))
}

/// `GET /backups/download?filename=<name>.zip` — raw zip bytes.
pub async fn download_backup(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<FilenameQuery>,
) -> Result<impl IntoResponse, ApiError> {
    if !state.is_authorized(&headers) {
        return Err(ApiError::unauthorized());
    }
    let name = crate::store::BackupStore::safe_filename(&q.filename).map_err(ApiError::from)?;
    let bytes = state.store.load(&name)?;
    Ok((
        [
            (header::CONTENT_TYPE, "application/zip".to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{name}\""),
            ),
        ],
        bytes,
    ))
}

/// `DELETE /backups/<name>.zip` — console-side removal.
pub async fn delete_backup(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(filename): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if !state.is_authorized(&headers) {
        return Err(ApiError::unauthorized());
    }
    state.store.delete(&filename)?;
    tracing::info!("deleted {filename}");
    Ok(Json(serde_json::json!({ "deleted": filename })))
}

// ---------------------------------------------------------------------------
// Client-facing pack distribution (SKCraft-style): NO auth — these endpoints
// are exactly what a public nginx repo-server exposes today, served straight
// from each registered pack's dist directory.
// ---------------------------------------------------------------------------

/// `GET /packs` — installable pack list shown by launcher clients.
pub async fn client_pack_list(
    State(state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let registry = crate::packs::PackRegistry::load(&state.data_dir)?;
    let mut packs = Vec::new();
    for p in &registry.packs {
        let status = crate::console::dist_status(&p.dist);
        if status["status"] != serde_json::json!("ok") {
            continue; // not-built / broken packs stay invisible to clients
        }
        packs.push(serde_json::json!({
            "id": p.id,
            "title": p.title,
            "game_version": status["game_version"],
            "pack_version": status["pack_version"],
            "loader": status["loader"],
            "file_count": status["file_count"],
            "total_bytes": status["total_bytes"],
        }));
    }
    Ok(Json(serde_json::json!({ "packs": packs })))
}

fn registered_dist(state: &AppState, id: &str) -> Result<std::path::PathBuf, ApiError> {
    let registry = crate::packs::PackRegistry::load(&state.data_dir)?;
    registry
        .find(id)
        .map(|p| p.dist.clone())
        .filter(|d| d.is_dir())
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, format!("pack not found: {id}")))
}

/// `GET /packs/:id/manifest.json` — the sync engine's manifest fetch.
pub async fn pack_manifest_file(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let dist = registered_dist(&state, &id)?;
    let bytes = std::fs::read(dist.join("manifest.json"))
        .map_err(|_| ApiError::new(StatusCode::NOT_FOUND, "pack has no manifest.json yet"))?;
    Ok(([(header::CONTENT_TYPE, "application/json")], bytes))
}

/// `GET /packs/:id/cas/objects/:shard/:hash` — one CAS blob.
pub async fn pack_blob(
    State(state): State<Arc<AppState>>,
    Path((id, shard, hash)): Path<(String, String, String)>,
) -> Result<impl IntoResponse, ApiError> {
    // The shard must be the hash's own prefix and the hash plain lowercase
    // hex, so the resolved path can never escape the pack's CAS directory.
    let valid_hash = hash.len() == 64
        && hash
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase());
    if !valid_hash || shard != &hash[..2] {
        return Err(ApiError::bad_request("invalid CAS object reference"));
    }
    let dist = registered_dist(&state, &id)?;
    let path = dist.join("cas").join("objects").join(shard).join(&hash);
    let bytes = std::fs::read(&path)
        .map_err(|_| ApiError::new(StatusCode::NOT_FOUND, "CAS object not found"))?;
    Ok(([(header::CONTENT_TYPE, "application/octet-stream")], bytes))
}
