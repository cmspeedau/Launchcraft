//! Launchcraft GUI client
//! A modern client-side launcher GUI with world save backups and Java settings.

mod java;
mod saves;

use eframe::egui;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

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
        Ok(std::fs::write(path, serde_json::to_vec_pretty(self)?)?)
    }

    pub fn load(path: &std::path::Path) -> Self {
        std::fs::read(path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }
}

pub struct App {
    pub settings_dir: PathBuf,
    pub java: JavaSettings,
    pub status: String,
    pub tab: Tab,
}

#[derive(PartialEq)]
pub enum Tab { Home, Worlds, Java, Logs }

impl App {
    fn settings_file(&self) -> PathBuf {
        self.settings_dir.join("settings.json")
    }
}

impl Default for App {
    fn default() -> Self {
        let dir = dirs();
        let app = Self {
            settings_dir: dir.clone(),
            java: JavaSettings::load(&dir.join("settings.json")),
            status: "Ready".into(),
            tab: Tab::Home,
        };
        app
    }
}

fn dirs() -> PathBuf {
    std::env::var("LAUNCHCRAFT_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            dirs_default().join(".launchcraft")
        })
}

fn dirs_default() -> PathBuf {
    #[cfg(target_os = "windows")]
    { std::env::var("APPDATA").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from(".")) }
    #[cfg(not(target_os = "windows"))]
    { std::env::var("HOME").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from(".")) }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.draw(ctx);
    }
}

impl App {
    fn draw(&mut self, ctx: &egui::Context) {
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
            ui.horizontal(|ui| { ui.label("Status:"); ui.label(&self.status); });
        });
        egui::CentralPanel::default().show(ctx, |ui| match self.tab {
            Tab::Home => self.draw_home(ui),
            Tab::Worlds => self.draw_worlds(ui),
            Tab::Java => self.draw_java(ui),
            Tab::Logs => self.draw_logs(ui),
        });
    }

    fn draw_home(&mut self, ui: &mut egui::Ui) {
        ui.add_space(20.0);
        ui.heading("Welcome to Launchcraft");
        ui.label("A modern Minecraft launcher.");
        ui.add_space(20.0);
        if ui.add(egui::Button::new("▶  PLAY").min_size(egui::vec2(160.0, 44.0))).clicked() {
            self.status = "Launching game...".into();
            // TODO: wire into launcher backend to start the game process
        }
    }

    fn draw_worlds(&mut self, ui: &mut egui::Ui) {
        ui.heading("World Save Backups");
        ui.separator();
        ui.horizontal(|ui| {
            if ui.button("Backup all worlds").clicked() {
                match saves::backup_all(&self.settings_dir) {
                    Ok(n) => self.status = format!("Backed up {n} world(s)"),
                    Err(e) => self.status = format!("Backup failed: {e}"),
                }
            }
            if ui.button("Open backups folder").clicked() {
                let _ = std::fs::create_dir_all(saves::backup_dir(&self.settings_dir));
                #[cfg(target_os = "windows")]
                let _ = std::process::Command::new("explorer")
                    .arg(saves::backup_dir(&self.settings_dir)).spawn();
                #[cfg(not(target_os = "windows"))]
                let _ = std::process::Command::new("xdg-open")
                    .arg(saves::backup_dir(&self.settings_dir)).spawn();
            }
        });
        ui.add_space(10.0);
        ui.label("Existing backups:");
        if let Ok(entries) = std::fs::read_dir(saves::backup_dir(&self.settings_dir)) {
            for e in entries.flatten() {
                ui.label(format!("📦 {}", e.file_name().to_string_lossy()));
            }
        }
    }

    fn draw_java(&mut self, ui: &mut egui::Ui) {
        ui.heading("Java Settings");
        ui.separator();
        egui::Grid::new("java_grid").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
            ui.label("Java executable:");
            ui.text_edit_singleline(&mut self.java.java_path);
            ui.end_row();

            ui.label("Min memory (MB):");
            ui.add(egui::DragValue::new(&mut self.java.min_memory_mb).range(256..=16384));
            ui.end_row();

            ui.label("Max memory (MB):");
            ui.add(egui::DragValue::new(&mut self.java.max_memory_mb).range(512..=32768));
            ui.end_row();

            ui.label("Extra JVM args:");
            ui.text_edit_singleline(&mut self.java.jvm_args);
            ui.end_row();
        });
        ui.add_space(8.0);
        ui.label(format!("Effective args: {}", self.java.to_jvm_args().join(" ")));
        ui.horizontal(|ui| {
            if ui.button("Save").clicked() {
                match self.java.save(&self.settings_file()) {
                    Ok(()) => self.status = "Java settings saved".into(),
                    Err(e) => self.status = format!("Save failed: {e}"),
                }
            }
            if ui.button("Detect Java").clicked() {
                self.java.java_path = java::detect_java().unwrap_or_else(|| "java".into());
            }
        });
    }

    fn draw_logs(&mut self, ui: &mut egui::Ui) {
        ui.heading("Logs");
        ui.label(&self.status);
    }
}

fn main() -> eframe::Result {
    let app = App::default();
    std::fs::create_dir_all(&app.settings_dir).ok();
    eframe::run_native(
        "Launchcraft",
        eframe::egui::vec2(960.0, 640.0),
        Box::new(move |_cc| Ok(Box::new(app))),
    )
}
