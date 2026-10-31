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
