//! CurseForge provider for the unified modpack browser (`modpacks.rs`).
//!
//! This server proxies the CurseForge Core API (`x-api-key` auth) so the
//! key never reaches a browser; every provider call lives behind the
//! console's `/console/modpacks/*` routes. CurseForge counts projects and
//! files in u64, so the unified string handles are parsed back here.

use axum::http::StatusCode;
use serde_json::Value;

use crate::modpacks::{fetch_bytes, safe_file_name, FallbackMeta, Found, PackVersion, SearchArgs};
use crate::{ApiError, AppState};

/// Minecraft on CurseForge.
const GAME_ID: u32 = 432;
/// The "Modpacks" class within Minecraft.
const MODPACK_CLASS_ID: u32 = 4471;
/// Search page size; the console pages by this.
pub(crate) const PAGE_SIZE: u32 = 20;

fn upstream(err: reqwest::Error) -> ApiError {
    ApiError::new(
        StatusCode::BAD_GATEWAY,
        format!("curseforge request failed: {err}"),
    )
}

/// GET a CurseForge Core API endpoint (`x-api-key` auth), unwrap the envelope.
async fn cf_get_json(
    state: &AppState,
    url: &str,
    query: &[(&str, String)],
) -> Result<Value, ApiError> {
    let key = state.curse_key.as_deref().unwrap_or_default();
    let res = state
        .http
        .get(url)
        .query(query)
        .header("x-api-key", key)
        .send()
        .await
        .map_err(upstream)?;
    let status = res.status();
    let body: Value = res
        .json()
        .await
        .map_err(|e| ApiError::new(StatusCode::BAD_GATEWAY, format!("curseforge reply: {e}")))?;
    if !status.is_success() {
        let msg = body
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("upstream error");
        let extra = if status == StatusCode::FORBIDDEN {
            " (check the API key)"
        } else {
            ""
        };
        return Err(ApiError::new(
            StatusCode::BAD_GATEWAY,
            format!("curseforge {status}: {msg}{extra}"),
        ));
    }
    Ok(body)
}

/// `//edge.forgecdn.net/x.png` → `https://edge.forgecdn.net/x.png`.
fn absolute_url(raw: &str) -> String {
    if let Some(rest) = raw.strip_prefix("//") {
        format!("https://{rest}")
    } else {
        raw.to_string()
    }
}

/// Parse one unified string handle back into CurseForge's numeric id.
fn numeric(raw: &str, what: &str) -> Result<u64, ApiError> {
    raw.trim().parse::<u64>().map_err(|_| {
        ApiError::bad_request(format!("curseforge {what} id must be numeric, got '{raw}'"))
    })
}

/// `GET /console/modpacks/search?provider=curseforge` backend.
pub(crate) async fn search(
    state: &AppState,
    args: &SearchArgs<'_>,
) -> Result<(Vec<Found>, u64), ApiError> {
    let mut query: Vec<(&str, String)> = vec![
        ("gameId", GAME_ID.to_string()),
        ("classId", MODPACK_CLASS_ID.to_string()),
        ("pageSize", PAGE_SIZE.to_string()),
        ("index", (args.index * PAGE_SIZE).to_string()),
        ("sortOrder", "desc".into()),
        (
            "sortField",
            match args.sort {
                "popularity" => "2",
                "lastupdated" => "3",
                "name" => "4",
                _ => "1", // featured
            }
            .to_string(),
        ),
    ];
    if let Some(q) = args.q {
        query.push(("searchTerms", q.to_string()));
    }
    if let Some(gv) = args.game_version {
        query.push(("gameVersions", gv.to_string()));
    }
    let body = cf_get_json(state, &format!("{}/mods/search", state.curse_base), &query).await?;

    let mut packs = Vec::new();
    if let Some(items) = body.get("data").and_then(Value::as_array) {
        for m in items {
            packs.push(Found {
                id: m
                    .get("id")
                    .and_then(Value::as_u64)
                    .unwrap_or_default()
                    .to_string(),
                name: m
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                description: m
                    .get("summary")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                icon: m
                    .pointer("/imageUrl/url")
                    .and_then(Value::as_str)
                    .map(absolute_url)
                    .unwrap_or_default(),
                downloads: m
                    .get("downloadCount")
                    .and_then(Value::as_u64)
                    .unwrap_or_default(),
            });
        }
    }
    let total = body
        .pointer("/pagination/totalCount")
        .and_then(Value::as_u64)
        .unwrap_or(packs.len() as u64);
    Ok((packs, total))
}

/// A project's files, newest release first (the API's own order varies).
async fn file_list(state: &AppState, project: u64) -> Result<Vec<Value>, ApiError> {
    let body = cf_get_json(
        state,
        &format!("{}/mods/{}/files", state.curse_base, project),
        &[("pageSize", "100".into())],
    )
    .await?;
    let mut files = body
        .get("data")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    files.sort_by(|a, b| {
        let da = a.get("releaseDate").and_then(Value::as_str).unwrap_or("");
        let db = b.get("releaseDate").and_then(Value::as_str).unwrap_or("");
        db.cmp(da)
    });
    Ok(files)
}

/// `GET /console/modpacks/versions?provider=curseforge` backend.
pub(crate) async fn versions(
    state: &AppState,
    project: &str,
) -> Result<Vec<PackVersion>, ApiError> {
    let project = numeric(project, "project")?;
    Ok(file_list(state, project)
        .await?
        .into_iter()
        .map(|f| PackVersion {
            id: f
                .get("id")
                .and_then(Value::as_u64)
                .unwrap_or_default()
                .to_string(),
            name: f
                .get("displayName")
                .and_then(Value::as_str)
                .or_else(|| f.get("fileName").and_then(Value::as_str))
                .unwrap_or_default()
                .to_string(),
            date: f
                .get("releaseDate")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            size: f
                .get("fileLength")
                .and_then(Value::as_u64)
                .unwrap_or_default(),
            game_versions: f
                .get("gameVersions")
                .and_then(Value::as_array)
                .map(|g| {
                    g.iter()
                        .filter_map(|v| v.as_str())
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default(),
        })
        .collect())
}

/// The project's newest file id (newest release date), as a string handle.
pub(crate) async fn latest(state: &AppState, project: &str) -> Result<String, ApiError> {
    let project = numeric(project, "project")?;
    file_list(state, project)
        .await?
        .first()
        .and_then(|f| f.get("id").and_then(Value::as_u64))
        .map(|id| id.to_string())
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::BAD_GATEWAY,
                "curseforge listed no files for this pack",
            )
        })
}

/// Fetch the modpack zip itself. CurseForge packs carry a full
/// `manifest.json`, so no fallback metadata is needed.
pub(crate) async fn download(
    state: &AppState,
    project: &str,
    file: &str,
) -> Result<(Vec<u8>, Option<FallbackMeta>), ApiError> {
    let project = numeric(project, "project")?;
    let file = numeric(file, "file")?;
    let dl = cf_get_json(
        state,
        &format!(
            "{}/mods/{}/files/{}/download",
            state.curse_base, project, file
        ),
        &[],
    )
    .await?;
    let cdn = dl
        .pointer("/data/cdnUrl")
        .and_then(Value::as_str)
        .filter(|u| !u.is_empty())
        .ok_or_else(|| ApiError::new(StatusCode::BAD_GATEWAY, "curseforge gave no cdnUrl"))?;
    Ok((fetch_bytes(state, cdn).await?, None))
}

/// Resolve one Forge-manifest reference (`projectID`/`fileID`) to the
/// `src`-relative path the mod should land at plus its download URL.
pub(crate) async fn resolve_mod_file(
    state: &AppState,
    project_id: u64,
    file_id: u64,
) -> Result<(String, String), ApiError> {
    let info = cf_get_json(
        state,
        &format!("{}/mods/{}/files/{}", state.curse_base, project_id, file_id),
        &[],
    )
    .await?;
    let url = info
        .pointer("/data/downloadUrl")
        .and_then(Value::as_str)
        .filter(|u| !u.is_empty())
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::BAD_GATEWAY,
                format!("no downloadUrl for mod {project_id} file {file_id}"),
            )
        })?
        .to_string();
    let name = info
        .pointer("/data/fileName")
        .and_then(Value::as_str)
        .and_then(safe_file_name)
        .unwrap_or_else(|| format!("mod-{project_id}-{file_id}.jar"));
    Ok((format!("mods/{name}"), url))
}
