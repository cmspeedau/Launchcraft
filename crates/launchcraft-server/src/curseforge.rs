//! CurseForge modpack browser + importer (server side only).
//!
//! The console talks to *this* server; this server proxies the CurseForge
//! Core API (`https://api.curseforge.com/v1`, `x-api-key` auth) so the key
//! never reaches a browser and CORS stays out of the picture.
//!
//! "Add" turns a CurseForge modpack into a launchcraft pack in one step:
//! download the modpack zip, read its `manifest.json` (Forge) or
//! `modrinth.index.json` (Fabric), fetch every referenced mod file into the
//! pack's `src/mods/`, merge `overrides/`, register the workspace and build
//! it — exactly the tree the Pack Manager would have you assemble by hand.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::{ApiError, AppState};

/// Minecraft on CurseForge.
const GAME_ID: u32 = 432;
/// The "Modpacks" class within Minecraft.
const MODPACK_CLASS_ID: u32 = 4471;

/// Whether browsing is possible and against which host.
pub async fn status(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    crate::console::auth(&state, &headers)?;
    Ok(Json(json!({
        "configured": state.curse_key.is_some(),
        "base": state.curse_base,
        "hint": "Set LAUNCHCRAFT_CURSEFORGE_KEY (a free key from \
                 https://developers.curseforge.org) to enable browsing.",
    })))
}

fn require_key(state: &AppState) -> Result<(), ApiError> {
    if state.curse_key.is_some() {
        Ok(())
    } else {
        Err(ApiError::bad_request(
            "CurseForge is not configured on this server: set LAUNCHCRAFT_CURSEFORGE_KEY \
             (free key from https://developers.curseforge.org) and restart",
        ))
    }
}

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

/// Download a file body (mod jar / modpack zip) with a hard size cap.
async fn download_bytes(state: &AppState, url: &str) -> Result<Vec<u8>, ApiError> {
    let mut res = state.http.get(url).send().await.map_err(upstream)?;
    if !res.status().is_success() {
        return Err(ApiError::new(
            StatusCode::BAD_GATEWAY,
            format!("download failed ({}): {url}", res.status()),
        ));
    }
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = res.chunk().await.map_err(upstream)? {
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

#[derive(Debug, Deserialize)]
pub struct SearchQuery {
    #[serde(default)]
    pub q: Option<String>,
    /// featured | popularity | lastupdated | name (CurseForge sortField 1..4).
    #[serde(default)]
    pub sort: Option<String>,
    /// Optional game-version filter, e.g. `1.20.1`.
    #[serde(default)]
    pub game_version: Option<String>,
    /// Zero-based result index for paging.
    #[serde(default)]
    pub index: Option<u32>,
}

/// `GET /console/curseforge/search` — modpack search, trimmed to what the
/// console list needs.
pub async fn search(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<SearchQuery>,
) -> Result<Json<Value>, ApiError> {
    crate::console::auth(&state, &headers)?;
    require_key(&state)?;
    let query: Vec<(&str, String)> = {
        let mut v = vec![
            ("gameId", GAME_ID.to_string()),
            ("classId", MODPACK_CLASS_ID.to_string()),
            ("pageSize", "30".into()),
            ("index", q.index.unwrap_or(0).to_string()),
            ("sortOrder", "desc".into()),
            (
                "sortField",
                match q.sort.as_deref() {
                    Some("popularity") => "2",
                    Some("lastupdated") => "3",
                    Some("name") => "4",
                    _ => "1", // featured
                }
                .to_string(),
            ),
        ];
        if let Some(terms) = q.q.as_deref().filter(|t| !t.trim().is_empty()) {
            v.push(("searchTerms", terms.trim().to_string()));
        }
        if let Some(gv) = q.game_version.as_deref().filter(|t| !t.trim().is_empty()) {
            v.push(("gameVersions", gv.trim().to_string()));
        }
        v
    };
    let body = cf_get_json(&state, &format!("{}/mods/search", state.curse_base), &query).await?;

    let packs: Vec<Value> = body
        .get("data")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .map(|m| {
                    let icon = m
                        .pointer("/imageUrl/url")
                        .and_then(Value::as_str)
                        .map(absolute_url)
                        .unwrap_or_default();
                    json!({
                        "id": m.get("id").and_then(Value::as_u64).unwrap_or_default(),
                        "name": m.get("name").and_then(Value::as_str).unwrap_or_default(),
                        "description": m
                            .get("summary")
                            .and_then(Value::as_str)
                            .unwrap_or_default(),
                        "icon": icon,
                        "downloads": m.get("downloadCount").and_then(Value::as_u64).unwrap_or_default(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let total = body
        .pointer("/pagination/totalCount")
        .and_then(Value::as_u64)
        .unwrap_or(packs.len() as u64);
    Ok(Json(json!({ "packs": packs, "total": total })))
}

/// `//edge.forgecdn.net/x.png` → `https://edge.forgecdn.net/x.png`.
fn absolute_url(raw: &str) -> String {
    if let Some(rest) = raw.strip_prefix("//") {
        format!("https://{rest}")
    } else {
        raw.to_string()
    }
}

#[derive(Debug, Deserialize)]
pub struct FilesQuery {
    pub mod_id: u64,
}

/// `GET /console/curseforge/files?mod_id=` — files for the "Version
/// selected" list, newest first.
pub async fn files(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(q): Query<FilesQuery>,
) -> Result<Json<Value>, ApiError> {
    crate::console::auth(&state, &headers)?;
    require_key(&state)?;
    let body = cf_get_json(
        &state,
        &format!("{}/mods/{}/files", state.curse_base, q.mod_id),
        &[("pageSize", "50".into())],
    )
    .await?;
    let mut versions: Vec<Value> = body
        .get("data")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .map(|f| {
                    json!({
                        "id": f.get("id").and_then(Value::as_u64).unwrap_or_default(),
                        "name": f
                            .get("displayName")
                            .and_then(Value::as_str)
                            .or_else(|| f.get("fileName").and_then(Value::as_str))
                            .unwrap_or_default(),
                        "file_name": f.get("fileName").and_then(Value::as_str).unwrap_or_default(),
                        "date": f.get("releaseDate").and_then(Value::as_str).unwrap_or_default(),
                        "size": f.get("fileLength").and_then(Value::as_u64).unwrap_or_default(),
                        "game_versions": f
                            .get("gameVersions")
                            .and_then(Value::as_array)
                            .map(|g| g.iter().filter_map(|v| v.as_str()).collect::<Vec<_>>())
                            .unwrap_or_default(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    versions.sort_by(|a, b| {
        b["date"]
            .as_str()
            .unwrap_or("")
            .cmp(a["date"].as_str().unwrap_or(""))
    });
    Ok(Json(json!({ "versions": versions })))
}

#[derive(Debug, Deserialize)]
pub struct AddBody {
    /// CurseForge project id.
    pub mod_id: u64,
    /// CurseForge file id for the chosen pack version.
    pub file_id: u64,
    /// launchcraft pack id for the registry.
    pub id: String,
    #[serde(default)]
    pub title: Option<String>,
}

/// `POST /console/curseforge/add` — download, unpack, fetch mods, build.
pub async fn add(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<AddBody>,
) -> Result<Json<Value>, ApiError> {
    crate::console::auth(&state, &headers)?;
    require_key(&state)?;
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

    // Download the modpack zip, fetch every referenced mod into src/mods/,
    // and merge the pack's overrides into the tree.
    let outcome = import_modpack(&state, body.mod_id, body.file_id, &src).await?;
    let plan = outcome.plan;

    // 5. pack.json + registry + build.
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
        curseforge: Some(crate::packs::CurseSource {
            project_id: body.mod_id,
            file_id: body.file_id,
        }),
    })?;
    registry.save(&state.data_dir)?;

    let built = crate::console::run_pack_build(src, dist, pack_id.clone(), None).await?;

    tracing::info!(
        "imported CurseForge pack '{pack_id}' (project {} file {}): {} file(s) in mods/",
        body.mod_id,
        body.file_id,
        outcome.fetched
    );
    Ok(Json(json!({
        "id": pack_id,
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

/// What one successful `import_modpack` run produced.
struct ImportOutcome {
    plan: ImportPlan,
    /// Mod files that landed in the tree.
    fetched: usize,
    /// Optional mods that could not be downloaded, described.
    warnings: Vec<String>,
    /// `src`-relative paths this import actually wrote (for update pruning).
    written: Vec<String>,
}

/// Download a CurseForge modpack file and (re)create its contents inside
/// `src`: fetch every referenced mod file into `src/mods/`, then merge the
/// pack's `overrides/` onto the tree. Shared by Add and Update; the staging
/// directory is removed on every path, errors included.
async fn import_modpack(
    state: &Arc<AppState>,
    mod_id: u64,
    file_id: u64,
    src: &Path,
) -> Result<ImportOutcome, ApiError> {
    // Ask the API where the modpack zip actually lives, then fetch it.
    let dl = cf_get_json(
        state,
        &format!(
            "{}/mods/{}/files/{}/download",
            state.curse_base, mod_id, file_id
        ),
        &[],
    )
    .await?;
    let cdn = dl
        .pointer("/data/cdnUrl")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::new(StatusCode::BAD_GATEWAY, "curseforge gave no cdnUrl"))?
        .to_string();
    let zip_bytes = download_bytes(state, &cdn).await?;

    // Unpack to staging (zip-slip safe), then read the pack manifest.
    let staging = state.data_dir.join("tmp").join(format!(
        "curse-{}",
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
    ));
    std::fs::create_dir_all(&staging)?;
    let plan = (|| -> Result<ImportPlan, ApiError> {
        extract_zip(&zip_bytes, &staging)?;
        read_plan(&staging)
    })();
    let plan = match plan {
        Ok(p) => p,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(e);
        }
    };

    // Fetch every referenced mod, then merge overrides over src/. The async
    // block lets a required-mod failure still fall through to cleanup.
    let mut fetched = 0usize;
    let mut written: Vec<String> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let fetch = async {
        std::fs::create_dir_all(src.join("mods"))?;
        std::fs::create_dir_all(src.join("config"))?;
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
                            let info = cf_get_json(
                                &st,
                                &format!(
                                    "{}/mods/{}/files/{}",
                                    st.curse_base, mf.project_id, mf.file_id
                                ),
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
                                        format!(
                                            "no downloadUrl for mod {} file {}",
                                            mf.project_id, mf.file_id
                                        ),
                                    )
                                })?
                                .to_string();
                            let name = info
                                .pointer("/data/fileName")
                                .and_then(Value::as_str)
                                .and_then(safe_file_name)
                                .unwrap_or_else(|| {
                                    format!("mod-{}-{}.jar", mf.project_id, mf.file_id)
                                });
                            (format!("mods/{name}"), url)
                        }
                    };
                    let bytes = download_bytes(&st, &url).await?;
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
            // Keep at most 8 concurrent fetches: polite to the CDN, simple code.
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
    Ok(ImportOutcome {
        plan,
        fetched,
        warnings,
        written,
    })
}

#[derive(Debug, Deserialize)]
pub struct UpdateBody {
    /// Explicit target file; defaults to the project's newest file.
    #[serde(default)]
    pub file_id: Option<u64>,
}

/// The newest file id of a CurseForge project (newest release date).
async fn latest_file_id(state: &AppState, project_id: u64) -> Result<u64, ApiError> {
    let body = cf_get_json(
        state,
        &format!("{}/mods/{}/files", state.curse_base, project_id),
        &[("pageSize", "50".into())],
    )
    .await?;
    body.get("data")
        .and_then(Value::as_array)
        .and_then(|items| {
            items
                .iter()
                .filter_map(|f| {
                    let id = f.get("id").and_then(Value::as_u64)?;
                    let date = f.get("releaseDate").and_then(Value::as_str).unwrap_or("");
                    Some((date.to_string(), id))
                })
                .max()
        })
        .map(|(_, id)| id)
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::BAD_GATEWAY,
                "curseforge listed no files for this pack",
            )
        })
}

/// Delete files under `src/mods` that the fresh import did not write, so
/// mods removed between CurseForge versions do not linger in the pack.
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

/// `POST /console/curseforge/update/:id` — re-import a CurseForge pack at a
/// newer project file and rebuild it. The registry title and pack id stay;
/// the mod list is rebuilt from the new manifest (stale mods pruned) and
/// overrides are re-merged. Body: optionally `{ "file_id": N }`.
pub async fn update(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    UrlPath(id): UrlPath<String>,
    body: Option<Json<UpdateBody>>,
) -> Result<Json<Value>, ApiError> {
    crate::console::auth(&state, &headers)?;
    require_key(&state)?;
    let mut registry = crate::packs::PackRegistry::load(&state.data_dir)?;
    let idx = registry
        .packs
        .iter()
        .position(|p| p.id == id)
        .ok_or_else(|| {
            ApiError::new(StatusCode::NOT_FOUND, format!("pack not registered: {id}"))
        })?;
    let entry = registry.packs[idx].clone();
    let source = entry.curseforge.clone().ok_or_else(|| {
        ApiError::bad_request(format!(
            "pack '{id}' was not imported from CurseForge, so there is nothing to update"
        ))
    })?;
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
    let requested = body.and_then(|Json(b)| b.file_id);
    let target = match requested {
        Some(f) => f,
        None => latest_file_id(&state, source.project_id).await?,
    };
    if target == source.file_id {
        return Ok(Json(json!({
            "id": id, "updated": false, "file_id": target,
            "note": "the pack already tracks this CurseForge file",
        })));
    }

    // Re-create the pack contents first; only once the new set has fully
    // landed are stale mods pruned, so a failed import breaks nothing.
    let outcome = import_modpack(&state, source.project_id, target, &src).await?;
    let pruned = prune_mods(&src, &outcome.written)?;

    // pack.json: same id + title, new version; then note the new file id.
    let mut config =
        launchcraft_builder::BuildConfig::load_or_default(Some(src.join("pack.json").as_path()));
    if !outcome.plan.version.trim().is_empty() {
        config.version = outcome.plan.version.clone();
    }
    if !outcome.plan.game_version.trim().is_empty() {
        config.game_version = outcome.plan.game_version.clone();
    }
    std::fs::write(src.join("pack.json"), serde_json::to_vec_pretty(&config)?)?;
    registry.packs[idx].curseforge = Some(crate::packs::CurseSource {
        project_id: source.project_id,
        file_id: target,
    });
    registry.save(&state.data_dir)?;

    let built = crate::console::run_pack_build(src, entry.dist, id.clone(), None).await?;
    tracing::info!(
        "updated CurseForge pack '{id}' (project {}) file {} -> {}",
        source.project_id,
        source.file_id,
        target
    );
    Ok(Json(json!({
        "id": id, "updated": true,
        "from_file": source.file_id, "file_id": target,
        "mods_fetched": outcome.fetched, "pruned": pruned, "warnings": outcome.warnings,
        "pack_version": built["pack_version"],
        "file_count": built["file_count"],
        "total_bytes": built["total_bytes"],
    })))
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
            tracing::warn!(
                "optional CurseForge mod {} skipped: {}",
                mf.file_id,
                e.message
            );
            warnings.push(format!(
                "optional mod {} skipped: {}",
                mf.file_id, e.message
            ));
        }
    }
    Ok(())
}

/// A referenced mod file inside a modpack manifest.
#[derive(Clone, Debug)]
struct ModFile {
    project_id: u64,
    file_id: u64,
    required: bool,
    /// `modrinth.index.json` packs give a path + direct URL; Forge packs
    /// resolve through the API instead and leave this None.
    direct: Option<(String, String)>,
}

struct ImportPlan {
    name: String,
    version: String,
    game_version: String,
    loader: String,
    mods: Vec<ModFile>,
    overrides: PathBuf,
}

/// Read `manifest.json` (Forge modpacks) or `modrinth.index.json` (Fabric).
fn read_plan(staging: &Path) -> Result<ImportPlan, ApiError> {
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
                .get("version")
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
        })
    } else {
        Err(ApiError::bad_request(
            "modpack zip has neither manifest.json nor modrinth.index.json",
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

/// Keep only the basename of a remote filename, or None if unusable.
fn safe_file_name(raw: &str) -> Option<String> {
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
