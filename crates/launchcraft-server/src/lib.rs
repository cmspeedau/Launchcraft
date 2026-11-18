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
pub mod forge;
pub mod ftb;
pub mod jres;
pub mod loader;
pub mod modpacks;
pub mod modrinth;
pub mod packs;
pub mod runtime_dist;
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
    /// Fabric loader meta API root. (`LAUNCHCRAFT_FABRIC_URL`)
    pub fabric_url: String,
    /// Quilt loader meta API root. (`LAUNCHCRAFT_QUILT_URL`)
    pub quilt_url: String,
    /// Forge Maven root for installer jars. (`LAUNCHCRAFT_FORGE_URL`)
    pub forge_url: String,
    /// NeoForge Maven root for installer jars. (`LAUNCHCRAFT_NEOFORGE_URL`)
    pub neoforge_url: String,
    /// Mojang metadata root used by server-side runtime builds; also honours
    /// `LAUNCHCRAFT_META_URL` the way the runtime crate does. (`LAUNCHCRAFT_META_URL`)
    pub meta_url: String,
    /// Mojang asset-object CDN root. (`LAUNCHCRAFT_RESOURCES_URL`)
    pub resources_url: String,
    /// Fetch every asset object into runtime distributions, not just the
    /// index. Off by default: a full vanilla asset store is hundreds of MB.
    /// (`LAUNCHCRAFT_RUNTIME_ASSETS`)
    pub runtime_assets: bool,
    /// Where managed Java runtimes live (`java-8/`, `java-17/`, …).
    /// Defaults to `<data_dir>/jres` so they persist on the data volume.
    /// (`LAUNCHCRAFT_JRES`)
    pub jres_dir: Option<PathBuf>,
    /// Adoptium API root used to provision Java majors.
    /// (`LAUNCHCRAFT_ADOPTIUM_URL`)
    pub adoptium_url: String,
    /// Reuse a JDK the host already has (`java` on PATH / `JAVA_HOME`) before
    /// downloading one. Set `LAUNCHCRAFT_JRES_HOST_JAVA=0` to keep the pool
    /// strictly managed (every major a known Temurin in `LAUNCHCRAFT_JRES`).
    pub jres_host_java: bool,
}

impl Default for ServerConfig {
    /// Production defaults — the values a bare `launchcraft-server` run uses
    /// before any `LAUNCHCRAFT_*` variable is applied.
    fn default() -> Self {
        Self {
            addr: "0.0.0.0:9090".into(),
            data_dir: PathBuf::from("launchcraft-server-data"),
            token: None,
            repo_dist: None,
            max_upload_mb: 256,
            curseforge_key: None,
            curseforge_url: "https://api.curseforge.com/v1".into(),
            modrinth_url: "https://api.modrinth.com/v2".into(),
            ftb_url: "https://dist.creeper.host/FTB2".into(),
            fabric_url: "https://meta.fabricmc.net/v2".into(),
            quilt_url: "https://meta.quiltmc.org/v3".into(),
            forge_url: "https://maven.minecraftforge.net".into(),
            neoforge_url: "https://maven.neoforged.net/releases".into(),
            meta_url: "https://piston-meta.mojang.com".into(),
            resources_url: "https://resources.download.minecraft.net".into(),
            runtime_assets: false,
            jres_dir: None,
            adoptium_url: "https://api.adoptium.net/v3".into(),
            jres_host_java: true,
        }
    }
}

impl ServerConfig {
    /// Defaults overridden by the environment. Empty values are ignored, so
    /// `LAUNCHCRAFT_CURSEFORGE_KEY=""` in a compose file keeps the provider
    /// disabled rather than sending an empty API key upstream.
    pub fn from_env() -> Self {
        let base = Self::default();
        let var = |key: &str| std::env::var(key).ok().filter(|v| !v.is_empty());
        let url = |key: &str, current: &str| var(key).unwrap_or_else(|| current.to_string());
        Self {
            addr: url("LAUNCHCRAFT_ADDR", &base.addr),
            data_dir: var("LAUNCHCRAFT_DATA_DIR")
                .map(PathBuf::from)
                .unwrap_or(base.data_dir),
            token: var("LAUNCHCRAFT_BACKUP_TOKEN").or(base.token),
            repo_dist: var("LAUNCHCRAFT_REPO_DIST")
                .map(PathBuf::from)
                .or(base.repo_dist),
            max_upload_mb: var("LAUNCHCRAFT_MAX_UPLOAD_MB")
                .and_then(|v| v.parse().ok())
                .unwrap_or(base.max_upload_mb),
            curseforge_key: var("LAUNCHCRAFT_CURSEFORGE_KEY").or(base.curseforge_key),
            curseforge_url: url("LAUNCHCRAFT_CURSEFORGE_URL", &base.curseforge_url),
            modrinth_url: url("LAUNCHCRAFT_MODRINTH_URL", &base.modrinth_url),
            ftb_url: url("LAUNCHCRAFT_FTB_URL", &base.ftb_url),
            fabric_url: url("LAUNCHCRAFT_FABRIC_URL", &base.fabric_url),
            quilt_url: url("LAUNCHCRAFT_QUILT_URL", &base.quilt_url),
            forge_url: url("LAUNCHCRAFT_FORGE_URL", &base.forge_url),
            neoforge_url: url("LAUNCHCRAFT_NEOFORGE_URL", &base.neoforge_url),
            meta_url: url("LAUNCHCRAFT_META_URL", &base.meta_url),
            resources_url: url("LAUNCHCRAFT_RESOURCES_URL", &base.resources_url),
            runtime_assets: var("LAUNCHCRAFT_RUNTIME_ASSETS")
                .map(|v| {
                    matches!(
                        v.to_ascii_lowercase().as_str(),
                        "1" | "true" | "full" | "yes"
                    )
                })
                .unwrap_or(base.runtime_assets),
            jres_dir: var("LAUNCHCRAFT_JRES").map(PathBuf::from).or(base.jres_dir),
            adoptium_url: url("LAUNCHCRAFT_ADOPTIUM_URL", &base.adoptium_url),
            jres_host_java: var("LAUNCHCRAFT_JRES_HOST_JAVA")
                .map(|v| {
                    !matches!(
                        v.to_ascii_lowercase().as_str(),
                        "0" | "false" | "no" | "off"
                    )
                })
                .unwrap_or(base.jres_host_java),
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
    /// Fabric loader meta API root (loader data fetch).
    pub fabric_base: String,
    /// Quilt loader meta API root (loader data fetch).
    pub quilt_base: String,
    /// Forge Maven root (installer jar fetch).
    pub forge_base: String,
    /// NeoForge Maven root (installer jar fetch).
    pub neoforge_base: String,
    /// Mojang metadata root (server-side runtime builds).
    pub meta_base: String,
    /// Mojang asset-object CDN root (server-side runtime builds).
    pub resources_base: String,
    /// Default asset behaviour for runtime builds (`false` = index only).
    pub runtime_assets: bool,
    /// Runtime registry, manifests and the shared runtime CAS.
    pub runtimes: runtime_dist::RuntimeStore,
    /// Serialises runtime registry writes and destructive runtime operations;
    /// builds themselves run detached in background tasks.
    pub runtime_lock: tokio::sync::Mutex<()>,
    /// Managed Java runtimes (`<data>/jres/java-<major>`), one per major so a
    /// 1.7.10 pack and a 1.20.5 pack can coexist.
    pub jres: jres::JreStore,
    /// Adoptium API root for provisioning Java majors.
    pub adoptium_url: String,
    /// Serialises Java registry writes and downloads.
    pub jre_lock: tokio::sync::Mutex<()>,
    /// Whether host JDKs (PATH / `JAVA_HOME`) may satisfy a Java requirement.
    pub jres_host_java: bool,
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
            fabric_base: cfg.fabric_url.trim_end_matches('/').to_string(),
            quilt_base: cfg.quilt_url.trim_end_matches('/').to_string(),
            forge_base: cfg.forge_url.trim_end_matches('/').to_string(),
            neoforge_base: cfg.neoforge_url.trim_end_matches('/').to_string(),
            meta_base: cfg.meta_url.trim_end_matches('/').to_string(),
            resources_base: cfg.resources_url.trim_end_matches('/').to_string(),
            runtime_assets: cfg.runtime_assets,
            runtimes: {
                let runtimes = runtime_dist::RuntimeStore::new(&cfg.data_dir);
                runtimes.ensure_layout()?;
                runtimes
            },
            runtime_lock: tokio::sync::Mutex::new(()),
            jres: {
                let store = jres::JreStore::new(
                    cfg.jres_dir
                        .clone()
                        .unwrap_or_else(|| cfg.data_dir.join("jres")),
                );
                store.ensure_layout()?;
                store
            },
            adoptium_url: cfg.adoptium_url.trim_end_matches('/').to_string(),
            jre_lock: tokio::sync::Mutex::new(()),
            jres_host_java: cfg.jres_host_java,
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
            runtime_id: None,
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
            .route("/runtimes", get(runtime_dist::client_runtime_list))
            .route(
                "/runtimes/:id/manifest.json",
                get(runtime_dist::runtime_manifest),
            )
            .route(
                "/runtimes/:id/cas/objects/:shard/:hash",
                get(runtime_dist::runtime_blob),
            )
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
        .route(
            "/console/modpacks/upload",
            post(modpacks::upload).route_layer(DefaultBodyLimit::max(max_upload_bytes)),
        )
        .route("/console/modpacks/update/:id", post(modpacks::update))
        .route(
            "/console/runtimes",
            get(runtime_dist::console_list).post(runtime_dist::console_build),
        )
        .route(
            "/console/runtimes/:id",
            delete(runtime_dist::console_delete),
        )
        .route("/console/runtimes/gc", post(runtime_dist::console_gc))
        .route(
            "/console/jres",
            get(jres::console_list).post(jres::console_provision),
        )
        .route("/console/jres/:major", delete(jres::console_delete))
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
