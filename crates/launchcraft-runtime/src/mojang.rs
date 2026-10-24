use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use crate::platform::{OperatingSystem, PlatformInfo};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadInfo {
    pub path: Option<String>,
    pub sha1: Option<String>,
    pub size: Option<u64>,
    pub url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LibraryDownloads {
    pub artifact: Option<DownloadInfo>,
    pub classifiers: Option<HashMap<String, DownloadInfo>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RuleAction {
    Allow,
    Disallow,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OsRule {
    pub name: Option<String>,
    pub version: Option<String>,
    pub arch: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rule {
    pub action: RuleAction,
    pub os: Option<OsRule>,
}

impl Rule {
    pub fn matches(&self, platform: &PlatformInfo) -> bool {
        if let Some(ref os_rule) = self.os {
            if let Some(ref name) = os_rule.name {
                let current_name = platform.mojang_os_name();
                if name != current_name {
                    return false;
                }
            }
        }
        true
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Library {
    pub name: String,
    pub downloads: Option<LibraryDownloads>,
    pub rules: Option<Vec<Rule>>,
    pub natives: Option<HashMap<String, String>>,
}

impl Library {
    pub fn is_applicable(&self, platform: &PlatformInfo) -> bool {
        let Some(rules) = &self.rules else {
            return true;
        };

        let mut allowed = false;
        for rule in rules {
            if rule.matches(platform) {
                allowed = rule.action == RuleAction::Allow;
            }
        }
        allowed
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssetIndexRef {
    pub id: String,
    pub sha1: String,
    pub size: u64,
    pub url: String,
    #[serde(rename = "totalSize")]
    pub total_size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VersionDownloads {
    pub client: DownloadInfo,
    pub server: Option<DownloadInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VersionManifest {
    pub id: String,
    #[serde(rename = "mainClass")]
    pub main_class: String,
    pub libraries: Vec<Library>,
    pub downloads: VersionDownloads,
    #[serde(rename = "assetIndex")]
    pub asset_index: Option<AssetIndexRef>,
    pub assets: Option<String>,
}

impl VersionManifest {
    pub fn resolve_classpath(
        &self,
        libraries_dir: &Path,
        client_jar: &Path,
        platform: &PlatformInfo,
    ) -> Vec<PathBuf> {
        let mut classpath = Vec::new();

        for lib in &self.libraries {
            if !lib.is_applicable(platform) {
                continue;
            }

            if let Some(ref downloads) = lib.downloads {
                if let Some(ref artifact) = downloads.artifact {
                    if let Some(ref path) = artifact.path {
                        classpath.push(libraries_dir.join(path));
                    }
                }
            }
        }

        classpath.push(client_jar.to_path_buf());
        classpath
    }
}
