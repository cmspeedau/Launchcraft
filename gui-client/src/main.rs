//! Launchcraft GUI client
//! A client-side launcher GUI with world save backups (local + server) and Java settings.

mod java;
mod saves;

use eframe::egui;
use launchcraft_core::instance::BackupConfig;
use launchcraft_core::paths::LauncherPaths;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JavaSettings {
    pub java_path: String,
    pub min_memory_mb: u32,
    pub max_memory_mb: u32,
    pub jvm_args: String,
}

impl Default for JavaSettings {
    fn default() -> Self {
        Self {
            java_path: java::detect_java().unwrap_or_else(|| "java".into()),
            min_memory_mb: 512,
            max_memory_mb: 4096,
            jvm_args: String::new(),
        }
    }
}

impl JavaSettings {
    pub fn to_jvm_args(&self) -> Vec<String> {
        let mut args = vec![
            format!("-Xms{}M", self.min_memory_mb),
            format!("-Xmx{}M", self.max_memory_mb),
        ];
        args.extend(self.jvm_args.split_whitespace().map(String::from));
        args
    }

    pub fn save(&self, path: &std::path::Path) -> std::io::Result<()> {
        std::fs::write(path, serde_json::to_vec_pretty(self)?)
    }

    pub fn load(path: &std::path::Path) -> Self {
        std::fs::read(path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }
}

pub struct App {
    pub paths: LauncherPaths,
    pub java: JavaSettings,
    pub status: String,
    pub log: Vec<String>,
    pub tab: Tab,
    pub instances: Vec<saves::InstanceEntry>,
    pub instance: Option<usize>,
    pub endpoint_input: String,
    endpoint_for: Option<usize>,
    pub busy: bool,
    runtime: tokio::runtime::Runtime,
    ctx: egui::Context,
    events_tx: Sender<saves::JobEvent>,
    events_rx: Receiver<saves::JobEvent>,
}

#[derive(PartialEq)]
pub enum Tab {
    Home,
    Worlds,
    Java,
    Logs,
}

impl App {
    pub fn new(paths: LauncherPaths, ctx: egui::Context) -> Self {
        let (events_tx, events_rx) = std::sync::mpsc::channel();
        let runtime = tokio::runtime::Runtime::new().expect("failed to start async runtime");
        let instances = saves::list_instances(&paths.instances);
        Self {
            java: JavaSettings::load(&paths.config.join("settings.json")),
            status: "Ready".into(),
            log: Vec::new(),
            tab: Tab::Home,
            instance: instances.first().map(|_| 0),
            endpoint_input: String::new(),
            endpoint_for: None,
            busy: false,
            paths,
            instances,
            runtime,
            ctx,
            events_tx,
            events_rx,
        }
    }

    fn settings_file(&self) -> PathBuf {
        self.paths.config.join("settings.json")
    }

    fn selected_instance(&self) -> Option<&saves::InstanceEntry> {
        self.instance.and_then(|i| self.instances.get(i))
    }

    fn push_log(&mut self, line: impl Into<String>) {
        let line = line.into();
        if self.log.len() >= 200 {
            self.log.remove(0);
        }
        self.log.push(line);
    }

    fn set_status(&mut self, line: impl Into<String>) {
        let line: String = line.into();
        self.status = line.clone();
        self.push_log(line);
    }

    /// Move finished/progressing background job lines into the status bar and log.
    fn drain_events(&mut self) {
        while let Ok(event) = self.events_rx.try_recv() {
            self.status = event.line.clone();
            self.push_log(event.line);
            if event.done {
                self.busy = false;
            }
        }
    }

    fn start_backup(&mut self, dir: PathBuf, config: BackupConfig, worlds: Vec<String>) {
        saves::spawn_backup_job(
            &self.runtime,
            self.ctx.clone(),
            self.events_tx.clone(),
            dir,
            config,
            worlds,
        );
        self.busy = true;
        self.set_status("Backing up worlds…");
    }

    fn start_upload(&mut self, dir: PathBuf, config: BackupConfig, archives: Vec<String>) {
        saves::spawn_upload_job(
            &self.runtime,
            self.ctx.clone(),
            self.events_tx.clone(),
            dir,
            config,
            archives,
        );
        self.busy = true;
        self.set_status("Uploading backups…");
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_events();

        egui::TopBottomPanel::top("nav").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("🚀 Launchcraft");
                ui.separator();
                ui.selectable_value(&mut self.tab, Tab::Home, "Home");
                ui.selectable_value(&mut self.tab, Tab::Worlds, "World Saves");
                ui.selectable_value(&mut self.tab, Tab::Java, "Java Settings");
                ui.selectable_value(&mut self.tab, Tab::Logs, "Logs");
            });
        });
        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.label("Status:");
                ui.strong(&self.status);
                if self.busy {
                    ui.spinner();
                }
            });
        });
        egui::CentralPanel::default().show(ctx, |ui| match self.tab {
            Tab::Home => self.draw_home(ui),
            Tab::Worlds => self.draw_worlds(ui),
            Tab::Java => self.draw_java(ui),
            Tab::Logs => self.draw_logs(ui),
        });
    }
}

impl App {
    fn draw_home(&mut self, ui: &mut egui::Ui) {
        ui.add_space(20.0);
        ui.heading("Welcome to Launchcraft");
        ui.label("A modern Minecraft launcher.");
        ui.label(format!("Data directory: {}", self.paths.root.display()));
        ui.add_space(20.0);
        if ui
            .add(egui::Button::new("▶  PLAY").min_size(egui::vec2(160.0, 44.0)))
            .clicked()
        {
            self.set_status("Launching game… (launcher backend not wired up yet)");
            // TODO: wire into launchcraft-runtime to start the game process
        }
    }

    fn draw_worlds(&mut self, ui: &mut egui::Ui) {
        ui.heading("World Save Backups");
        ui.separator();

        if self.instances.is_empty() {
            ui.label(format!(
                "No instances found under {}.",
                self.paths.instances.display()
            ));
            ui.label("Install a modpack instance to enable world backups.");
            return;
        }

        let current = self
            .selected_instance()
            .map(|i| i.display_name().to_string())
            .unwrap_or_else(|| "Select an instance…".into());
        egui::ComboBox::from_id_source("instance_picker")
            .selected_text(current)
            .show_ui(ui, |ui| {
                for (i, entry) in self.instances.iter().enumerate() {
                    ui.selectable_value(&mut self.instance, Some(i), entry.display_name().to_string());
                }
            });
        if ui.button("Rescan instances").clicked() {
            self.instances = saves::list_instances(&self.paths.instances);
            self.instance = self.instances.first().map(|_| 0);
            self.endpoint_for = None;
        }

        let Some(idx) = self.instance.filter(|i| self.instances.get(*i).is_some()) else {
            ui.label("Select an instance above.");
            return;
        };
        let entry = &self.instances[idx];
        let dir = entry.dir.clone();
        let config = entry.config.backup_config.clone();
        let cloud = entry.cloud_upload_possible();

        let policy = if !config.server_backups_enabled {
            "Cloud uploads: disabled by server policy".to_string()
        } else {
            match config.endpoint_url.as_deref() {
                Some(endpoint) => format!("Cloud uploads: enabled → {endpoint}"),
                None => "Cloud uploads: enabled, but no endpoint URL configured".to_string(),
            }
        };
        ui.label(policy);

        // Where the backup server lives — stored per instance in its instance.json.
        if self.endpoint_for != Some(idx) {
            self.endpoint_input = config.endpoint_url.clone().unwrap_or_default();
            self.endpoint_for = Some(idx);
        }
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label("Backup server URL:");
            ui.add_sized(
                [320.0, ui.spacing().interact_size.y.max(20.0)],
                egui::TextEdit::singleline(&mut self.endpoint_input)
                    .hint_text("https://backups.example.com/api"),
            );
            if ui.button("Save server").clicked() {
                match saves::set_endpoint(&dir, &self.endpoint_input) {
                    Ok(url) => {
                        if let Some(entry) = self.instances.get_mut(idx) {
                            entry.config.backup_config.endpoint_url = url.clone();
                        }
                        match url {
                            Some(u) => self.set_status(format!("Backup server set to {u}")),
                            None => self.set_status("Backup server cleared"),
                        }
                    }
                    Err(e) => self.set_status(format!("Could not save server URL: {e}")),
                }
            }
        });

        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.add_enabled_ui(!self.busy, |ui| {
                if ui.button("Backup all worlds").clicked() {
                    self.start_backup(dir.clone(), config.clone(), Vec::new());
                }
            });
            if ui.button("Open backups folder").clicked() {
                let backups = saves::backups_dir(&dir);
                let _ = std::fs::create_dir_all(&backups);
                open_in_file_manager(&backups);
            }
        });

        ui.add_space(8.0);
        ui.strong("Worlds");
        let worlds = saves::worlds(&dir);
        if worlds.is_empty() {
            ui.weak("No world saves found for this instance.");
        }
        egui::ScrollArea::vertical().id_source("worlds_scroll").max_height(200.0).show(ui, |ui| {
            for world in &worlds {
                ui.horizontal(|ui| {
                    let lock = if world.is_locked { "  🔒" } else { "" };
                    ui.label(format!(
                        "🌍 {}{lock}  ({})",
                        world.display_name,
                        saves::human_size(world.size_bytes)
                    ));
                    let label = if cloud { "Backup + Upload" } else { "Backup" };
                    ui.add_enabled_ui(!self.busy && !world.is_locked, |ui| {
                        if ui.button(label).clicked() {
                            self.start_backup(
                                dir.clone(),
                                config.clone(),
                                vec![world.folder_name.clone()],
                            );
                        }
                    });
                });
            }
        });

        ui.add_space(8.0);
        ui.strong("Existing backups");
        let backups = saves::local_backups(&dir);
        if backups.is_empty() {
            ui.weak("No local backup archives yet.");
        }
        egui::ScrollArea::vertical().id_source("backups_scroll").max_height(180.0).show(ui, |ui| {
            for backup in &backups {
                ui.horizontal(|ui| {
                    ui.label(format!(
                        "📦 {}  ({})",
                        backup.file_name,
                        saves::human_size(backup.size_bytes)
                    ));
                    ui.add_enabled_ui(!self.busy && cloud, |ui| {
                        if ui.button("Upload").clicked() {
                            self.start_upload(
                                dir.clone(),
                                config.clone(),
                                vec![backup.file_name.clone()],
                            );
                        }
                    });
                    if ui
                        .add_enabled(!self.busy, egui::Button::new("Restore"))
                        .clicked()
                    {
                        match saves::restore(&dir, &backup.file_name) {
                            Ok(path) => self.set_status(format!("Restored {}", path.display())),
                            Err(e) => self.set_status(format!("Restore failed: {e}")),
                        }
                    }
                });
            }
        });
    }

    fn draw_java(&mut self, ui: &mut egui::Ui) {
        ui.heading("Java Settings");
        ui.separator();
        egui::Grid::new("java_grid").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
            ui.label("Java executable:");
            ui.text_edit_singleline(&mut self.java.java_path);
            ui.end_row();

            ui.label("Min memory (MB):");
            ui.add(egui::DragValue::new(&mut self.java.min_memory_mb).clamp_range(256..=16384));
            ui.end_row();

            ui.label("Max memory (MB):");
            ui.add(egui::DragValue::new(&mut self.java.max_memory_mb).clamp_range(512..=32768));
            ui.end_row();

            ui.label("Extra JVM args:");
            ui.text_edit_singleline(&mut self.java.jvm_args);
            ui.end_row();
        });
        ui.add_space(8.0);
        ui.label(format!("Effective args: {}", self.java.to_jvm_args().join(" ")));
        ui.label(format!("Saved to: {}", self.settings_file().display()));
        ui.horizontal(|ui| {
            if ui.button("Save").clicked() {
                match self.java.save(&self.settings_file()) {
                    Ok(()) => self.set_status("Java settings saved"),
                    Err(e) => self.set_status(format!("Save failed: {e}")),
                }
            }
            if ui.button("Detect Java").clicked() {
                match java::detect_java() {
                    Some(path) => {
                        self.java.java_path = path;
                        match java::version(&self.java.java_path) {
                            Some(v) => self.set_status(format!("Detected {v}")),
                            None => self.set_status("Java path set, but the version query failed"),
                        }
                    }
                    None => self.set_status("No Java runtime found — set the path manually"),
                }
            }
        });
    }

    fn draw_logs(&mut self, ui: &mut egui::Ui) {
        ui.heading("Logs");
        ui.separator();
        egui::ScrollArea::vertical()
            .id_source("logs_scroll")
            .auto_shrink([false, false])
            .stick_to_bottom(true)
            .show(ui, |ui| {
                if self.log.is_empty() {
                    ui.weak("Nothing logged yet.");
                }
                for line in &self.log {
                    ui.monospace(line);
                }
            });
    }
}

fn open_in_file_manager(path: &std::path::Path) {
    #[cfg(target_os = "windows")]
    let opener = "explorer";
    #[cfg(target_os = "macos")]
    let opener = "open";
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let opener = "xdg-open";
    let _ = std::process::Command::new(opener).arg(path).spawn();
}

fn main() -> eframe::Result<()> {
    let paths = LauncherPaths::resolve().expect("could not resolve launcher data directories");
    paths.ensure_dirs().ok();
    eframe::run_native(
        "Launchcraft",
        eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default().with_inner_size([960.0, 640.0]),
            ..Default::default()
        },
        Box::new(move |cc| Box::new(App::new(paths, cc.egui_ctx.clone()))),
    )
}
