//! The PLAY path: refresh the account token when needed, turn the selected
//! installed version into a launch plan, spawn the game and stream its logs.

use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::sync::Arc;

use eframe::egui;
use launchcraft_auth::{LaunchcraftAccount, MicrosoftAuthClient};
use launchcraft_core::instance::InstanceConfig;
use launchcraft_core::paths::LauncherPaths;
use launchcraft_runtime::meta::{LaunchIdentity, LaunchSpec, LoaderKind, MinecraftClient};
use launchcraft_runtime::{
    JavaDownloadProgress, JavaInstallPhase, JavaProgress, JavaRuntimeManager, PlatformInfo,
    ProcessSpawner,
};

use crate::saves::{report, report_progress, JobEvent};

/// Lifecycle notices for the Home tab's PLAY button.
#[derive(Debug, Clone)]
pub enum GameEvent {
    /// Java process is up; `pid` when the OS reported one.
    Started { pid: Option<u32> },
    /// Launch never happened (planning or spawn error).
    Failed(String),
    /// The game process exited (or was killed).
    Stopped,
}

pub struct LaunchRequest {
    pub paths: LauncherPaths,
    pub version_id: String,
    pub instance_dir: PathBuf,
    pub java_binary: PathBuf,
    pub min_memory_mb: u32,
    pub max_memory_mb: u32,
    pub jvm_extra: Vec<String>,
    pub account: LaunchcraftAccount,
}

/// Launch id an instance's game files correspond to on disk: plain vanilla
/// or the `<mc>-<loader>-<version>` overlay the Versions tab installs.
/// Forge/NeoForge packs ship their loader files inside the synced instance
/// and cannot launch through this path yet.
pub fn instance_launch_id(config: &InstanceConfig) -> Result<String, String> {
    let mc = config.minecraft_version.clone();
    match &config.mod_loader {
        None => Ok(mc),
        Some(loader) => {
            let tag = format!("{}-{}", loader.loader_type, loader.loader_version);
            match parse_loader_tag(&tag) {
                Some((kind, version)) => Ok(format!("{mc}-{}-{version}", kind.id())),
                None => Err(format!(
                    "{} instances ship their loader inside the pack; \
                     launcher-managed {} launches are not supported yet",
                    loader.loader_type, loader.loader_type
                )),
            }
        }
    }
}

/// Parse the server's `loader` tag (e.g. `"fabric-0.15.11"`) for instance.json.
pub fn parse_loader_tag(tag: &str) -> Option<(LoaderKind, String)> {
    for (prefix, kind) in [
        ("fabric-", LoaderKind::Fabric),
        ("quilt-", LoaderKind::Quilt),
    ] {
        if let Some(rest) = tag.strip_prefix(prefix) {
            if !rest.is_empty() {
                return Some((kind, rest.to_string()));
            }
        }
    }
    None
}

/// On-disk install id a server pack corresponds to: vanilla `game_version`
/// or the `<mc>-<loader>-<version>` overlay the Versions tab installs.
/// Loader kinds we cannot manage (forge/neoforge) are refused by name.
pub fn pack_launch_id(game_version: &str, loader: Option<&str>) -> Result<String, String> {
    let tag = loader.unwrap_or_default();
    if tag.is_empty() {
        return Ok(game_version.to_string());
    }
    match parse_loader_tag(tag) {
        Some((kind, version)) => Ok(format!("{game_version}-{}-{version}", kind.id())),
        None => Err(format!(
            "packs with loader '{tag}' are not launchable through the version \
             manager yet — fabric and quilt are supported"
        )),
    }
}

/// Choose the `java` executable for a launch. Priority: a previously
/// provisioned managed runtime for the profile's major version; the user's
/// configured Java when it is new enough; otherwise download the matching
/// Temurin build into `runtimes/java-<major>` (Prism-style auto-provisioning).
/// A failed download falls back to the configured Java with a warning.
async fn pick_java(
    req: &LaunchRequest,
    required: Option<u32>,
    job_tx: &Sender<JobEvent>,
    ctx: &egui::Context,
) -> PathBuf {
    let user_java = req.java_binary.clone();
    let Some(major) = required else {
        return user_java;
    };
    let manager = JavaRuntimeManager::new(req.paths.runtimes.clone());
    if let Some(managed) = manager.provisioned_java(major) {
        return managed;
    }
    let probe = user_java.clone();
    let user_major = tokio::task::spawn_blocking(move || crate::java::major_of(&probe))
        .await
        .unwrap_or(None);
    if user_major.is_some_and(|m| m >= major) {
        return user_java;
    }
    report_progress(
        job_tx,
        ctx,
        format!(
            "☕ This version needs Java {major} (configured is {}); downloading Temurin…",
            user_major
                .map(|m| m.to_string())
                .unwrap_or_else(|| "undetectable".into())
        ),
        Some(0.0),
        false,
    );
    let progress = java_progress_bridge(job_tx, ctx, major);
    match manager.ensure_java(major, Some(&progress)).await {
        Ok(path) => {
            report_progress(
                job_tx,
                ctx,
                format!("✅ Installed managed Java {major}"),
                Some(1.0),
                true,
            );
            path
        }
        Err(e) => {
            report(
                job_tx,
                ctx,
                format!(
                    "⚠ Could not install Java {major}: {e} — launching with the configured Java"
                ),
                true,
            );
            user_java
        }
    }
}

/// Bridge byte-level Java install progress into throttled job events
/// (≈0.5 % granularity, like the version installer).
fn java_progress_bridge(tx: &Sender<JobEvent>, ctx: &egui::Context, major: u32) -> JavaProgress {
    let (tx, ctx) = (tx.clone(), ctx.clone());
    let last = Arc::new(std::sync::Mutex::new((String::new(), -1.0f32)));
    Arc::new(move |p: JavaDownloadProgress| {
        let total = p.total.unwrap_or(0);
        let frac = if total > 0 {
            (p.downloaded as f32 / total as f32).min(1.0)
        } else {
            0.0
        };
        let phase = match p.phase {
            JavaInstallPhase::Checking => "checking",
            JavaInstallPhase::Downloading => "downloading",
            JavaInstallPhase::Extracting => "extracting",
            JavaInstallPhase::Done => "done",
        };
        let mut last = last.lock().unwrap_or_else(|e| e.into_inner());
        let fresh = last.0 != phase || frac >= last.1 + 0.005 || p.phase == JavaInstallPhase::Done;
        if !fresh {
            return;
        }
        *last = (phase.to_string(), frac);
        report_progress(
            &tx,
            &ctx,
            format!("☕ Java {major} {phase} ({:.0}%)", frac * 100.0),
            Some(frac),
            false,
        );
    })
}

/// Refresh the Minecraft token when it lapses soon, then plan + spawn.
/// `log_tx` receives every game console line as raw `String`s.
pub fn spawn_launch(
    rt: &tokio::runtime::Runtime,
    ctx: egui::Context,
    job_tx: Sender<JobEvent>,
    game_tx: Sender<GameEvent>,
    log_tx: Sender<String>,
    account_tx: Sender<LaunchcraftAccount>,
    req: LaunchRequest,
) {
    rt.spawn(async move {
        // Fresh token (also persists through the account tab via account_tx).
        let account = if req.account.expires_soon(300) {
            let auth = MicrosoftAuthClient::new(std::env::var("LAUNCHCRAFT_CLIENT_ID").ok());
            match auth.refresh_account(&req.account).await {
                Ok(fresh) => {
                    let _ = account_tx.send(fresh.clone());
                    fresh
                }
                Err(e) => {
                    let msg = format!("Token refresh failed: {e}");
                    report(&job_tx, &ctx, msg.clone(), true);
                    let _ = game_tx.send(GameEvent::Failed(msg));
                    ctx.request_repaint();
                    return;
                }
            }
        } else {
            req.account.clone()
        };

        let client = MinecraftClient::new(req.paths.clone());
        let platform = PlatformInfo::current();
        let spec = LaunchSpec {
            version_id: &req.version_id,
            instance_dir: &req.instance_dir,
            java_binary: req.java_binary.clone(),
            min_memory_mb: req.min_memory_mb,
            max_memory_mb: req.max_memory_mb,
            jvm_extra: req.jvm_extra.clone(),
            identity: LaunchIdentity {
                username: account.username.clone(),
                uuid: account.uuid.simple().to_string(),
                token: account.access_token.clone(),
                xuid: account.xuid.clone(),
                client_id: std::env::var("LAUNCHCRAFT_CLIENT_ID")
                    .unwrap_or_else(|_| launchcraft_auth::oauth::DEFAULT_CLIENT_ID.to_string()),
            },
        };
        let mut plan = match client.plan_launch(&spec, &platform) {
            Ok(plan) => plan,
            Err(e) => {
                let msg = format!("Could not prepare launch: {e}");
                report(&job_tx, &ctx, msg.clone(), true);
                let _ = game_tx.send(GameEvent::Failed(msg));
                ctx.request_repaint();
                return;
            }
        };

        // Prism-style auto-provisioning: when the profile wants a newer Java
        // than the configured one, download a matching Temurin runtime and
        // swap it into the plan before spawning.
        plan.config.java_binary = pick_java(&req, plan.java_major, &job_tx, &ctx).await;

        let mut process = match ProcessSpawner::spawn(plan.config, platform, move |line| {
            let _ = log_tx.send(line);
        }) {
            Ok(p) => p,
            Err(e) => {
                let msg = format!("Failed to start Java: {e}");
                report(&job_tx, &ctx, msg.clone(), true);
                let _ = game_tx.send(GameEvent::Failed(msg));
                ctx.request_repaint();
                return;
            }
        };

        report(
            &job_tx,
            &ctx,
            format!("🚀 Launching {} as {}", req.version_id, account.username),
            false,
        );
        let _ = game_tx.send(GameEvent::Started { pid: process.id() });
        ctx.request_repaint();

        match process.wait().await {
            Ok(status) => report(&job_tx, &ctx, format!("Minecraft exited ({status})"), false),
            Err(e) => report(
                &job_tx,
                &ctx,
                format!("Minecraft process error: {e}"),
                false,
            ),
        }
        let _ = game_tx.send(GameEvent::Stopped);
        ctx.request_repaint();
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use launchcraft_core::instance::{InstanceConfig, InstanceSource, ModLoaderConfig};

    fn config(loader: Option<ModLoaderConfig>) -> InstanceConfig {
        InstanceConfig {
            id: uuid::Uuid::new_v4(),
            name: "i".into(),
            minecraft_version: "1.20.4".into(),
            mod_loader: loader,
            java_path: None,
            jvm_args: Vec::new(),
            source: InstanceSource::Manual {
                created_at: chrono::Utc::now(),
            },
            icon_path: None,
            backup_config: Default::default(),
        }
    }

    #[test]
    fn loader_tags_parse() {
        assert_eq!(
            parse_loader_tag("fabric-0.15.11"),
            Some((LoaderKind::Fabric, "0.15.11".into()))
        );
        assert_eq!(
            parse_loader_tag("quilt-0.21.0"),
            Some((LoaderKind::Quilt, "0.21.0".into()))
        );
        assert_eq!(parse_loader_tag("forge-43.2.0"), None);
        assert_eq!(parse_loader_tag("fabric-"), None);
    }

    #[test]
    fn pack_launch_ids_match_the_versions_tab_layout() {
        assert_eq!(pack_launch_id("1.20.4", None).unwrap(), "1.20.4");
        assert_eq!(
            pack_launch_id("1.20.4", Some("fabric-0.15.11")).unwrap(),
            "1.20.4-fabric-0.15.11"
        );
        assert_eq!(
            pack_launch_id("1.20.4", Some("quilt-0.21.0")).unwrap(),
            "1.20.4-quilt-0.21.0"
        );
        assert!(pack_launch_id("1.20.4", Some("forge-43.2.0"))
            .unwrap_err()
            .contains("forge"));
    }

    #[test]
    fn instance_launch_ids_cover_all_loaders() {
        assert_eq!(instance_launch_id(&config(None)).unwrap(), "1.20.4");
        assert_eq!(
            instance_launch_id(&config(Some(ModLoaderConfig {
                loader_type: "fabric".into(),
                loader_version: "0.15.11".into(),
            })))
            .unwrap(),
            "1.20.4-fabric-0.15.11"
        );
        assert!(instance_launch_id(&config(Some(ModLoaderConfig {
            loader_type: "neoforge".into(),
            loader_version: "4.0.1".into(),
        })))
        .is_err());
    }
}
