//! Minecraft version browser: pulls the Mojang version list, lets the user
//! install vanilla versions (with assets) and add fabric/quilt loaders on
//! top. Installs stream progress into the shared job channel.

use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;

use eframe::egui;
use launchcraft_core::paths::LauncherPaths;
use launchcraft_runtime::meta::{
    InstallProgress, LoaderKind, MinecraftClient, VersionListEntry, VersionListFile,
};

use crate::saves::{report, report_progress, JobEvent};

/// Which loader to layer on a vanilla install (none = plain vanilla).
#[derive(Debug, Clone, PartialEq)]
pub enum LoaderChoice {
    None,
    Fabric,
    Quilt,
}

impl LoaderChoice {
    pub fn kind(self) -> Option<LoaderKind> {
        match self {
            LoaderChoice::None => None,
            LoaderChoice::Fabric => Some(LoaderKind::Fabric),
            LoaderChoice::Quilt => Some(LoaderKind::Quilt),
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            LoaderChoice::None => "vanilla",
            LoaderChoice::Fabric => "Fabric",
            LoaderChoice::Quilt => "Quilt",
        }
    }
}

/// State of the Versions tab.
pub struct VersionState {
    pub list: Option<VersionListFile>,
    pub loading: bool,
    pub error: Option<String>,
    pub show_snapshots: bool,
    pub filter: String,
    /// Selected version id from the (filtered) list.
    pub selected: Option<String>,
    pub loader: LoaderChoice,
    /// Loader versions for the selected Minecraft version (newest first).
    pub loader_versions: Vec<String>,
    pub loader_pick: Option<String>,
    pub loaders_loading: bool,
    /// The version id `loader_versions` currently belongs to.
    pub loaders_for: Option<String>,
    /// Ids currently installed on disk (profile json + client jar).
    pub installed: Vec<String>,
    /// The id whose install is running right now.
    pub installing: Option<String>,
    list_tx: Sender<Result<VersionListFile, String>>,
    list_rx: Receiver<Result<VersionListFile, String>>,
    loaders_tx: Sender<Result<(String, Vec<String>), String>>,
    loaders_rx: Receiver<Result<(String, Vec<String>), String>>,
}

impl VersionState {
    pub fn new() -> Self {
        let (list_tx, list_rx) = std::sync::mpsc::channel();
        let (loaders_tx, loaders_rx) = std::sync::mpsc::channel();
        Self {
            list: None,
            loading: false,
            error: None,
            show_snapshots: false,
            filter: String::new(),
            selected: None,
            loader: LoaderChoice::None,
            loader_versions: Vec::new(),
            loader_pick: None,
            loaders_loading: false,
            loaders_for: None,
            installed: Vec::new(),
            installing: None,
            list_tx,
            list_rx,
            loaders_tx,
            loaders_rx,
        }
    }

    pub fn list_tx(&self) -> Sender<Result<VersionListFile, String>> {
        self.list_tx.clone()
    }

    pub fn loaders_tx(&self) -> Sender<Result<(String, Vec<String>), String>> {
        self.loaders_tx.clone()
    }

    /// Fold finished list/loader fetches into the state.
    pub fn drain(&mut self) {
        while let Ok(event) = self.list_rx.try_recv() {
            self.loading = false;
            match event {
                Ok(list) => {
                    self.error = None;
                    let preferred = list.latest.release.clone();
                    if !matches!(self.selected.as_deref(), Some(id) if list.find(id).is_some()) {
                        self.selected = Some(preferred);
                    }
                    self.list = Some(list);
                }
                Err(e) => self.error = Some(e),
            }
        }
        while let Ok(event) = self.loaders_rx.try_recv() {
            self.loaders_loading = false;
            match event {
                Ok((for_id, versions)) => {
                    if self.selected.as_deref() == Some(for_id.as_str()) {
                        self.loader_versions = versions;
                        self.loader_pick = self.loader_versions.first().cloned();
                    }
                }
                Err(e) => self.error = Some(e),
            }
        }
    }

    /// Fetch loader list (fabric/quilt) for the selected version when kind changes.
    pub fn request_loaders(
        &mut self,
        rt: &tokio::runtime::Runtime,
        ctx: egui::Context,
        paths: LauncherPaths,
        kind: LoaderKind,
    ) {
        let Some(id) = self.selected.clone() else {
            return;
        };
        if self.loaders_for.as_deref() == Some(id.as_str()) && !self.loader_versions.is_empty() {
            return;
        }
        self.loaders_for = Some(id.clone());
        self.loader_versions.clear();
        self.loader_pick = None;
        self.loaders_loading = true;
        let tx = self.loaders_tx();
        spawn_loaders(rt, ctx, tx, paths, kind, id);
    }

    pub fn rescan_installed(&mut self, paths: &LauncherPaths) {
        self.installed = MinecraftClient::new(paths.clone()).installed_ids();
    }

    /// The filtered list of versions to render (release-only by default).
    pub fn visible(&self) -> Vec<&VersionListEntry> {
        let Some(list) = &self.list else {
            return Vec::new();
        };
        let filter = self.filter.trim().to_lowercase();
        list.versions
            .iter()
            .filter(|v| self.show_snapshots || v.kind == "release")
            .filter(|v| filter.is_empty() || v.id.to_lowercase().contains(&filter))
            .collect()
    }
}

/// Fetch the Mojang version list in the background.
pub fn spawn_list(
    rt: &tokio::runtime::Runtime,
    ctx: egui::Context,
    tx: Sender<Result<VersionListFile, String>>,
    paths: LauncherPaths,
) {
    rt.spawn(async move {
        let result = MinecraftClient::new(paths)
            .version_list()
            .await
            .map_err(|e| e.to_string());
        let _ = tx.send(result);
        ctx.request_repaint();
    });
}

/// Fetch loader versions for one Minecraft version in the background.
pub fn spawn_loaders(
    rt: &tokio::runtime::Runtime,
    ctx: egui::Context,
    tx: Sender<Result<(String, Vec<String>), String>>,
    paths: LauncherPaths,
    kind: LoaderKind,
    mc_version: String,
) {
    rt.spawn(async move {
        let result = MinecraftClient::new(paths)
            .loader_versions(kind, &mc_version)
            .await
            .map(|v| (mc_version.clone(), v))
            .map_err(|e| e.to_string());
        let _ = tx.send(result);
        ctx.request_repaint();
    });
}

/// Install a version (optionally with a loader overlay) into the shared
/// versions/assets/libraries store, streaming `InstallProgress` into job
/// events with a completion fraction.
pub fn spawn_install(
    rt: &tokio::runtime::Runtime,
    ctx: egui::Context,
    tx: Sender<JobEvent>,
    paths: LauncherPaths,
    version_id: String,
    loader: Option<(LoaderKind, String)>,
) {
    let label = match &loader {
        Some((kind, lver)) => format!("{version_id} + {} {lver}", kind.id()),
        None => version_id.clone(),
    };
    rt.spawn(async move {
        report_progress(&tx, &ctx, format!("Installing {label}…"), Some(0.0), false);
        let client = MinecraftClient::new(paths);
        let progress = install_progress_bridge(&tx, &ctx);
        let result: Result<launchcraft_runtime::InstallReport, String> = async {
            let list = client.version_list().await.map_err(|e| e.to_string())?;
            match &loader {
                None => {
                    let entry = list
                        .find(&version_id)
                        .cloned()
                        .ok_or_else(|| format!("'{version_id}' is not in the version manifest"))?;
                    client
                        .install_version(&entry, &list, true, Some(&progress))
                        .await
                }
                Some((kind, lver)) => {
                    client
                        .install_loader(*kind, &version_id, lver, &list, true, Some(&progress))
                        .await
                }
            }
            .map_err(|e| e.to_string())
        }
        .await;
        match result {
            Ok(r) => report_progress(
                &tx,
                &ctx,
                format!(
                    "✅ {label}: {} file(s), {:.1} MB downloaded",
                    r.files_written,
                    r.bytes_downloaded as f64 / 1_048_576.0
                ),
                Some(1.0),
                true,
            ),
            Err(e) => report(&tx, &ctx, format!("⚠ Install of {label} failed: {e}"), true),
        }
    });
}

/// Wrap the channel into a progress callback; reports are throttled to
/// one per ~0.5 % of phase progress to keep the event queue small.
fn install_progress_bridge(
    tx: &Sender<JobEvent>,
    ctx: &egui::Context,
) -> Arc<dyn Fn(InstallProgress) + Send + Sync> {
    let (tx, ctx) = (tx.clone(), ctx.clone());
    let last = Arc::new(std::sync::Mutex::new((String::new(), -1.0f32)));
    Arc::new(move |p: InstallProgress| {
        let frac = if p.total > 0 {
            (p.done as f32 / p.total as f32).min(1.0)
        } else {
            0.0
        };
        let mut last = last.lock().unwrap_or_else(|e| e.into_inner());
        let fresh = last.0 != p.phase || frac >= last.1 + 0.005 || p.done == p.total;
        if !fresh {
            return;
        }
        *last = (p.phase.to_string(), frac);
        report_progress(
            &tx,
            &ctx,
            format!(
                "⬇ {} — {}/{} — {}",
                p.phase,
                p.done,
                p.total,
                if p.item.is_empty() {
                    ""
                } else {
                    p.item.as_str()
                }
            ),
            Some(frac),
            false,
        );
    })
}
