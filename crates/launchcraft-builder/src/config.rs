use std::path::{Path, PathBuf};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildConfig {
    pub pack_id: String,
    pub title: String,
    pub version: String,
    pub game_version: String,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default = "default_user_overrides")]
    pub user_overrides: Vec<String>,
    #[serde(default = "default_zstd_level")]
    pub compression_level: i32,
}

fn default_user_overrides() -> Vec<String> {
    vec![
        "options.txt".to_string(),
        "optionsof.txt".to_string(),
        "servers.dat".to_string(),
        "saves/".to_string(),
        "screenshots/".to_string(),
        "resourcepacks/".to_string(),
        "shaderpacks/".to_string(),
        "logs/".to_string(),
        "crash-reports/".to_string(),
    ]
}

fn default_zstd_level() -> i32 {
    3
}

impl Default for BuildConfig {
    fn default() -> Self {
        Self {
            pack_id: "custom-pack".to_string(),
            title: "Custom Modpack".to_string(),
            version: "1.0.0".to_string(),
            game_version: "1.20.4".to_string(),
            summary: Some("Compiled by Launchcraft Pack Builder".to_string()),
            user_overrides: default_user_overrides(),
            compression_level: default_zstd_level(),
        }
    }
}

impl BuildConfig {
    pub fn load_or_default(config_path: Option<&Path>) -> Self {
        if let Some(path) = config_path {
            if path.exists() {
                if let Ok(content) = std::fs::read_to_string(path) {
                    if let Ok(cfg) = serde_json::from_str(&content) {
                        return cfg;
                    }
                }
            }
        }
        Self::default()
    }
}
