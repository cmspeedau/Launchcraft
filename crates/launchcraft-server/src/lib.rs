//! Launchcraft server-side console: world backup API + admin GUI.
//!
//! Implements the HTTP contract that `launchcraft-backup::BackupSyncClient`
//! speaks (`POST /backups/upload?filename=…`, `GET /backups/download?filename=…`),
//! adds listing/deletion endpoints, and serves an embedded admin console.
//! Routes are mounted both under `/api` (matching an `endpoint_url` of
//! `http://host:port/api`) and at the root, so either form works.

pub mod api;
pub mod console;
pub mod curseforge;
pub mod ftb;
pub mod modpacks;
pub mod modrinth;
pub mod packs;
pub mod store;

use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::DefaultBodyLimit;
use axum::http::header::AUTHORIZATION;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::Deserialize;
use thiserror::Error;

use crate::store::{BackupStore, StoreError};

/// Runtime configuration, sourced from environment variables.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// Listen address, e.g. `0.0.0.0:9090`. (`LAUNCHCRAFT_ADDR`)
    pub addr: String,
    /// Data directory holding `backups/` and `policy.json`. (`LAUNCHCRAFT_DATA_DIR`)
    pub data_dir: PathBuf,
    /// Optional bearer token required by every data endpoint. (`LAUNCHCRAFT_BACKUP_TOKEN`)
    pub token: Option<String>,
    /// Optional built pack directory (`manifest.json` + `cas/`) for the Packs view. (`LAUNCHCRAFT_REPO_DIST`)
    pub repo_dist: Option<PathBuf>,
    /// Per-upload body cap. (`LAUNCHCRAFT_MAX_UPLOAD_MB`)
    pub max_upload_mb: u32,
    /// CurseForge Core API key enabling the console's modpack browser. (`LAUNCHCRAFT_CURSEFORGE_KEY`)
    pub curseforge_key: Option<String>,
    /// CurseForge API root; swap for a mirror in tests. (`LAUNCHCRAFT_CURSEFORGE_URL`)
    pub curseforge_url: String,
    /// Modrinth API root; keyless provider. (`LAUNCHCRAFT_MODRINTH_URL`)
    pub modrinth_url: String,
    /// FTB legacy pack CDN root; keyless provider. (`LAUNCHCRAFT_FTB_URL`)
    pub ftb_url: String,
}

impl ServerConfig {
    pub fn from_env() -> Self {
        Self {
            addr: std::env::var("LAUNCHCRAFT_ADDR").unwrap_or_else(|_| "0.0.0.0:9090".into()),
            data_dir: std::env::var("LAUNCHCRAFT_DATA_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from("launchcraft-server-data")),
            token: std::env::var("LAUNCHCRAFT_BACKUP_TOKEN")
                .ok()
                .filter(|t| !t.is_empty()),
            repo_dist: std::env::var("LAUNCHCRAFT_REPO_DIST")
                .ok()
                .filter(|d| !d.is_empty())
                .map(PathBuf::from),
            max_upload_mb: std::env::var("LAUNCHCRAFT_MAX_UPLOAD_MB")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(256),
            curseforge_key: std::env::var("LAUNCHCRAFT_CURSEFORGE_KEY")
                .ok()
                .filter(|k| !k.is_empty()),
            curseforge_url: std::env::var("LAUNCHCRAFT_CURSEFORGE_URL")
                .ok()
                .filter(|u| !u.is_empty())
                .unwrap_or_else(|| "https://api.curseforge.com/v1".into()),
            modrinth_url: std::env::var("LAUNCHCRAFT_MODRINTH_URL")
                .ok()
                .filter(|u| !u.is_empty())
                .unwrap_or_else(|| "https://api.modrinth.com/v2".into()),
            ftb_url: std::env::var("LAUNCHCRAFT_FTB_URL")
                .ok()
                .filter(|u| !u.is_empty())
                .unwrap_or_else(|| "https://dist.creeper.host/FTB2".into()),
        }
    }
}

/// Shared handler state.
pub struct AppState {
    pub store: BackupStore,
    /// Root for `packs.json` and scaffolded pack workspaces.
    pub data_dir: PathBuf,
    pub token: Option<String>,
    pub repo_dist: Option<PathBuf>,
    pub max_upload_bytes: usize,
    /// CurseForge Core API key (`None` disables that provider in the browser).
    pub curse_key: Option<String>,
    /// CurseForge API root used by the browser.
    pub curse_base: String,
    /// Modrinth API root (keyless provider).
    pub modrinth_base: String,
    /// FTB legacy CDN root (keyless provider).
    pub ftb_base: String,
    /// Shared outbound HTTP client for the modpack providers.
    pub http: reqwest::Client,
}

impl AppState {
    pub fn new(cfg: &ServerConfig) -> anyhow::Result<Arc<Self>> {
        let store = BackupStore::new(&cfg.data_dir);
        store.ensure_layout()?;
        let state = Arc::new(Self {
            store,
            data_dir: cfg.data_dir.clone(),
            token: cfg.token.clone(),
            repo_dist: cfg.repo_dist.clone(),
            max_upload_bytes: cfg.max_upload_mb.max(1) as usize * 1024 * 1024,
            curse_key: cfg.curseforge_key.clone(),
            curse_base: cfg.curseforge_url.trim_end_matches('/').to_string(),
            modrinth_base: cfg.modrinth_url.trim_end_matches('/').to_string(),
            ftb_base: cfg.ftb_url.trim_end_matches('/').to_string(),
            http: reqwest::Client::builder()
                .user_agent("LaunchcraftServer/0.1 (self-hosted modpack server)")
                .build()
                .unwrap_or_default(),
        });
        state.seed_registry_from_repo_dist()?;
        Ok(state)
    }

    /// First-boot convenience: when no registry exists yet and `LAUNCHCRAFT_REPO_DIST`
    /// holds a built pack, register it automatically so the Pack Manager is not
    /// empty on a single-pack deployment.
    fn seed_registry_from_repo_dist(&self) -> anyhow::Result<()> {
        if packs::PackRegistry::path(&self.data_dir).exists() {
            return Ok(());
        }
        let Some(dist) = self.repo_dist.as_ref() else {
            return Ok(());
        };
        let Ok(text) = std::fs::read_to_string(dist.join("manifest.json")) else {
            return Ok(());
        };
        let Ok(manifest) = serde_json::from_str::<launchcraft_core::manifest::PackManifest>(&text)
        else {
            return Ok(());
        };
        let mut registry = packs::PackRegistry::default();
        if packs::validate_pack_id(&manifest.pack_id).is_err() {
            return Ok(());
        }
        registry.packs.push(packs::PackEntry {
            id: manifest.pack_id.clone(),
            title: manifest.pack_id.clone(),
            src: None,
            dist: dist.clone(),
            created_at: chrono::Utc::now().to_rfc3339(),
            source: None,
        });
        registry.save(&self.data_dir)?;
        tracing::info!(
            "registered pack '{}' from LAUNCHCRAFT_REPO_DIST",
            manifest.pack_id
        );
        Ok(())
    }

    pub fn is_authorized(&self, headers: &HeaderMap) -> bool {
        let Some(token) = &self.token else {
            return true; // no token configured: open API (LAN / dev default)
        };
        headers
            .get(AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(|provided| provided == token)
            .unwrap_or(false)
    }
}

/// Typed handler error: JSON body `{ "message": … }` matching the
/// `BackupSyncClient::RemoteServer { status, message }` surfacing.
#[derive(Debug, Error)]
#[error("{message}")]
pub struct ApiError {
    pub status: StatusCode,
    pub message: String,
}

impl ApiError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }
    fn unauthorized() -> Self {
        Self::new(StatusCode::UNAUTHORIZED, "missing or invalid bearer token")
    }
    fn forbidden(message: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, message)
    }
}

impl From<StoreError> for ApiError {
    fn from(err: StoreError) -> Self {
        match err {
            StoreError::InvalidFilename(name) => {
                Self::bad_request(format!("invalid archive filename: {name}"))
            }
            StoreError::NotFound(name) => {
                Self::new(StatusCode::NOT_FOUND, format!("archive not found: {name}"))
            }
            StoreError::Io(e) => Self::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
            StoreError::Json(e) => Self::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        }
    }
}

impl From<crate::packs::RegistryError> for ApiError {
    fn from(err: crate::packs::RegistryError) -> Self {
        match err {
            crate::packs::RegistryError::InvalidId(_)
            | crate::packs::RegistryError::AlreadyRegistered(_) => {
                Self::bad_request(err.to_string())
            }
            crate::packs::RegistryError::Unknown(_) => {
                Self::new(StatusCode::NOT_FOUND, err.to_string())
            }
            crate::packs::RegistryError::Io(e) => {
                Self::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
            }
            crate::packs::RegistryError::Json(e) => {
                Self::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
            }
        }
    }
}

impl From<std::io::Error> for ApiError {
    fn from(err: std::io::Error) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string())
    }
}

impl From<serde_json::Error> for ApiError {
    fn from(err: serde_json::Error) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(serde_json::json!({ "message": self.message })),
        )
            .into_response()
    }
}

/// A `?filename=` query parameter, shared by upload and download.
#[derive(Debug, Deserialize)]
pub struct FilenameQuery {
    pub filename: String,
}

/// Build the full application router for a shared state.
pub fn build_router(state: Arc<AppState>) -> Router {
    let max_upload_bytes = state.max_upload_bytes;
    let api_routes = || {
        Router::new()
            .route("/health", get(|| async { "ok" }))
            .route("/backups", get(api::list_backups))
            .route(
                "/backups/upload",
                post(api::upload_backup).route_layer(DefaultBodyLimit::max(max_upload_bytes)),
            )
            .route("/backups/download", get(api::download_backup))
            .route("/backups/:filename", delete(api::delete_backup))
            .route("/packs", get(api::client_pack_list))
            .route("/packs/:id/manifest.json", get(api::pack_manifest_file))
            .route("/packs/:id/cas/objects/:shard/:hash", get(api::pack_blob))
    };
    let console_routes: axum::Router<()> = Router::new()
        .route("/", get(console::index))
        .route("/console", get(console::index))
        .route(
            "/console/policy",
            get(console::get_policy).post(console::set_policy),
        )
        .route("/console/stats", get(console::stats))
        .route(
            "/console/packs",
            get(console::packs_list).post(console::packs_add),
        )
        .route("/console/packs/new", post(console::packs_new))
        .route(
            "/console/packs/:id",
            axum::routing::put(console::packs_modify).delete(console::packs_remove),
        )
        .route("/console/packs/:id/check", post(console::packs_check))
        .route("/console/packs/:id/build", post(console::packs_build))
        .route("/console/modpacks", get(modpacks::providers))
        .route("/console/modpacks/search", get(modpacks::search))
        .route("/console/modpacks/versions", get(modpacks::versions))
        .route("/console/modpacks/add", post(modpacks::add))
        .route("/console/modpacks/update/:id", post(modpacks::update))
        .with_state(state.clone());
    let api: axum::Router<()> = api_routes().with_state(state);
    Router::new()
        .merge(console_routes)
        .merge(api.clone())
        .nest("/api", api)
}

/// Bind, serve, and run until Ctrl-C.
pub async fn serve(cfg: ServerConfig) -> anyhow::Result<()> {
    let state = AppState::new(&cfg)?;
    let app = build_router(state);
    let listener = tokio::net::TcpListener::bind(&cfg.addr).await?;
    let local = listener.local_addr()?;
    tracing::info!(
        "launchcraft-server listening on http://{local} (data: {}, token: {})",
        cfg.data_dir.display(),
        if cfg.token.is_some() {
            "required"
        } else {
            "off"
        }
    );
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}
