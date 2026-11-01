//! Modrinth provider for the unified modpack browser (`modpacks.rs`).
//!
//! Keyless public API (`https://api.modrinth.com/v2`); a project is named
//! by its slug and a version by its short id. Modpack files are `.mrpack`
//! archives: a zip holding `modrinth.index.json`, whose file entries carry
//! direct download URLs, so the importer never calls back into this module.

use axum::http::StatusCode;
use serde_json::Value;

use crate::modpacks::{fetch_bytes, FallbackMeta, Found, PackVersion, SearchArgs};
use crate::{ApiError, AppState};

/// Search page size; the console pages by this.
pub(crate) const PAGE_SIZE: u32 = 20;

/// GET a Modrinth v2 endpoint and parse its JSON reply.
async fn mr_get_json(
    state: &AppState,
    path: &str,
    query: &[(&str, String)],
) -> Result<Value, ApiError> {
    let res = state
        .http
        .get(format!("{}{path}", state.modrinth_base))
        .query(query)
        .send()
        .await
        .map_err(|e| {
            ApiError::new(
                StatusCode::BAD_GATEWAY,
                format!("modrinth request failed: {e}"),
            )
        })?;
    let status = res.status();
    let body: Value = res.json().await.map_err(|e| {
        ApiError::new(
            StatusCode::BAD_GATEWAY,
            format!("modrinth reply (is the slug valid?): {e}"),
        )
    })?;
    if !status.is_success() {
        let msg = body
            .get("error")
            .or_else(|| body.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("upstream error");
        return Err(ApiError::new(
            StatusCode::BAD_GATEWAY,
            format!("modrinth {status}: {msg}"),
        ));
    }
    Ok(body)
}

/// The `.mrpack` (primary) file's download URL, else the first file.
fn primary(v: &Value) -> Option<&Value> {
    let files = v.get("files").and_then(Value::as_array)?;
    files
        .iter()
        .find(|f| f.get("primary").and_then(Value::as_bool).unwrap_or(false))
        .or_else(|| files.first())
}

/// `GET /console/modpacks/search?provider=modrinth` backend.
pub(crate) async fn search(
    state: &AppState,
    args: &SearchArgs<'_>,
) -> Result<(Vec<Found>, u64), ApiError> {
    // Facets AND together: always modpacks, optionally a game version.
    let facets = match args.game_version {
        Some(gv) => format!("[[\"project_type:modpack\"],[\"versions:{gv}\"]]"),
        None => "[[\"project_type:modpack\"]]".to_string(),
    };
    let query = [
        ("query", args.q.unwrap_or("").to_string()),
        ("limit", PAGE_SIZE.to_string()),
        ("offset", (args.index * PAGE_SIZE).to_string()),
        (
            "index",
            match args.sort {
                "popularity" => "downloads",
                "lastupdated" => "updated",
                _ => "relevance", // featured and name both ride relevance
            }
            .to_string(),
        ),
        ("facets", facets),
    ];
    let body = mr_get_json(state, "/search", &query).await?;

    let mut packs = Vec::new();
    if let Some(hits) = body.get("hits").and_then(Value::as_array) {
        for h in hits {
            packs.push(Found {
                id: h
                    .get("slug")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                name: h
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                description: h
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                icon: h
                    .get("icon_url")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                downloads: h
                    .get("downloads")
                    .and_then(Value::as_u64)
                    .unwrap_or_default(),
            });
        }
    }
    let total = body
        .get("total_hits")
        .and_then(Value::as_u64)
        .unwrap_or(packs.len() as u64);
    Ok((packs, total))
}

/// `GET /console/modpacks/versions?provider=modrinth` backend, newest first.
pub(crate) async fn versions(
    state: &AppState,
    project: &str,
) -> Result<Vec<PackVersion>, ApiError> {
    let project = project.trim();
    let body = mr_get_json(state, &format!("/project/{project}/version"), &[]).await?;
    let mut versions: Vec<PackVersion> = body
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|v| PackVersion {
            id: v
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            name: v
                .get("name")
                .and_then(Value::as_str)
                .or_else(|| v.get("version_number").and_then(Value::as_str))
                .unwrap_or_default()
                .to_string(),
            date: v
                .get("date_published")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            size: primary(&v)
                .and_then(|f| f.get("size").and_then(Value::as_u64))
                .unwrap_or_default(),
            game_versions: v
                .get("game_versions")
                .and_then(Value::as_array)
                .map(|g| {
                    g.iter()
                        .filter_map(|x| x.as_str())
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default(),
        })
        .collect();
    versions.sort_by(|a, b| b.date.cmp(&a.date));
    Ok(versions)
}

/// Newest published version id of a project.
pub(crate) async fn latest(state: &AppState, project: &str) -> Result<String, ApiError> {
    versions(state, project)
        .await?
        .into_iter()
        .next()
        .map(|v| v.id)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::BAD_GATEWAY,
                "modrinth listed no versions for this pack",
            )
        })
}

/// Fetch the `.mrpack` archive for one version id.
pub(crate) async fn download(
    state: &AppState,
    _project: &str,
    file: &str,
) -> Result<(Vec<u8>, Option<FallbackMeta>), ApiError> {
    let body = mr_get_json(state, &format!("/version/{}", file.trim()), &[]).await?;
    let url = primary(&body)
        .and_then(|f| f.get("url").and_then(Value::as_str))
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::BAD_GATEWAY,
                "modrinth version has no downloadable file",
            )
        })?
        .to_string();
    Ok((fetch_bytes(state, &url).await?, None))
}
