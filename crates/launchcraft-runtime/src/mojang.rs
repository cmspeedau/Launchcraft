use crate::platform::{Architecture, PlatformInfo};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

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
    /// Feature requirements (`is_demo_enabled`, …). Launchcraft never
    /// enables optional features, so any rule demanding one never matches.
    #[serde(default)]
    pub features: Option<HashMap<String, serde_json::Value>>,
}

impl Rule {
    pub fn matches(&self, platform: &PlatformInfo) -> bool {
        if self.features.as_ref().is_some_and(|f| !f.is_empty()) {
            return false;
        }
        if let Some(ref os_rule) = self.os {
            if let Some(ref name) = os_rule.name {
                if name != platform.mojang_os_name() {
                    return false;
                }
            }
            if let Some(ref arch) = os_rule.arch {
                let current = match platform.arch {
                    Architecture::AArch64 => "arm64",
                    Architecture::X86 => "x86",
                    _ => "x64",
                };
                if arch != current {
                    return false;
                }
            }
            // OS-version regex rules gate ancient JVM workarounds; matching
            // them conservatively (never) is the safe call.
            if os_rule.version.is_some() {
                return false;
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
    /// Extractor directive from old Forge-era manifests; unsupported and
    /// treated as "this library is not applicable" when present.
    #[serde(default)]
    pub extract: Option<serde_json::Value>,
}

impl Library {
    pub fn is_applicable(&self, platform: &PlatformInfo) -> bool {
        if self.extract.is_some() {
            return false;
        }
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

    /// The natives classifier this library contributes on `platform`,
    /// with `${arch}` already substituted (`natives-linux`, …).
    pub fn natives_classifier(&self, platform: &PlatformInfo) -> Option<String> {
        let map = self.natives.as_ref()?;
        let value = map.get(platform.mojang_os_name())?;
        let arch = match platform.arch {
            Architecture::AArch64 => "arm64",
            Architecture::Arm => "arm32",
            Architecture::X86 => "32",
            _ => "64",
        };
        Some(value.replace("${arch}", arch))
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

/// One entry of a `assetIndexes` object file: maps virtual paths to hashed
/// object names inside `assets/objects/`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssetObject {
    pub hash: String,
    pub size: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssetIndex {
    pub objects: HashMap<String, AssetObject>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VersionDownloads {
    pub client: DownloadInfo,
    pub server: Option<DownloadInfo>,
}

/// One game/jvm argument: a plain string, or a value gated by rules
/// (features, OS) — the modern `arguments` block of a version profile.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum GameArg {
    Plain(String),
    Conditional { rules: Vec<Rule>, value: ArgValue },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ArgValue {
    One(String),
    Many(Vec<String>),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Arguments {
    pub game: Option<Vec<GameArg>>,
    pub jvm: Option<Vec<GameArg>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JavaVersionInfo {
    #[serde(default)]
    pub component: Option<String>,
    #[serde(default, rename = "majorVersion")]
    pub major_version: Option<u32>,
}

/// One layer of the version picture: a vanilla or loader (fabric/quilt)
/// version profile as served by Mojang / loader meta APIs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VersionManifest {
    pub id: String,
    #[serde(rename = "mainClass")]
    pub main_class: String,
    /// Loader overlays and modded profiles inherit the vanilla layer under
    /// this id; their own file omits downloads/assets entirely.
    #[serde(default, rename = "inheritsFrom")]
    pub inherits_from: Option<String>,
    pub libraries: Vec<Library>,
    #[serde(default)]
    pub arguments: Option<Arguments>,
    /// Pre-1.13 single-string game arguments.
    #[serde(default, rename = "minecraftArguments")]
    pub minecraft_arguments: Option<String>,
    #[serde(default, rename = "javaVersion")]
    pub java_version: Option<JavaVersionInfo>,
    /// Loader overlay profiles carry no downloads block of their own.
    #[serde(default)]
    pub downloads: Option<VersionDownloads>,
    #[serde(rename = "assetIndex")]
    pub asset_index: Option<AssetIndexRef>,
    pub assets: Option<String>,
}

/// A fully inherited launch picture built from a chain of profiles,
/// ordered base (vanilla) first, loader overlay last.
#[derive(Debug, Clone)]
pub struct MergedProfile {
    pub id: String,
    pub main_class: String,
    /// Overlay libraries first (a loader must precede the game on the
    /// classpath), then base libraries.
    pub libraries: Vec<Library>,
    pub game_args: GameArgs,
    pub asset_index: Option<AssetIndexRef>,
    pub assets: Option<String>,
    pub java_component: Option<String>,
    pub java_major: Option<u32>,
    /// The base layer's client jar download (overlays never carry one).
    pub client_download: Option<DownloadInfo>,
}

/// How the game's own command line is spelled: the modern rule-gated
/// `arguments` block (concatenated across the chain, overlays first) or
/// the legacy single `minecraftArguments` string.
#[derive(Debug, Clone)]
pub enum GameArgs {
    Modern(Vec<GameArg>),
    Legacy(String),
}

impl MergedProfile {
    /// Merge profile layers, `layers[0]` being the deepest parent.
    pub fn merge(layers: &[VersionManifest]) -> Option<MergedProfile> {
        let last = layers.last()?;
        let mut libraries = Vec::new();
        let mut modern = Vec::new();
        let mut saw_modern = false;
        // Overlays first: both classpath order and argument overrides favor
        // the child profile.
        for layer in layers.iter().rev() {
            libraries.extend(layer.libraries.iter().cloned());
            if let Some(args) = &layer.arguments {
                if let Some(game) = &args.game {
                    modern.extend(game.iter().cloned());
                    saw_modern = true;
                }
            }
        }
        let game_args = if saw_modern {
            GameArgs::Modern(modern)
        } else {
            let legacy = layers
                .iter()
                .rev()
                .find_map(|l| l.minecraft_arguments.clone())
                .unwrap_or_default();
            GameArgs::Legacy(legacy)
        };
        let base = layers.first()?;
        Some(MergedProfile {
            id: last.id.clone(),
            main_class: last.main_class.clone(),
            libraries,
            game_args,
            // Newest layer wins, falling back to the base version's assets.
            asset_index: layers.iter().rev().find_map(|l| l.asset_index.clone()),
            java_component: layers
                .iter()
                .rev()
                .find_map(|l| l.java_version.as_ref()?.component.clone()),
            java_major: layers
                .iter()
                .rev()
                .find_map(|l| l.java_version.as_ref()?.major_version),
            client_download: base.downloads.as_ref().map(|d| d.client.clone()),
            assets: base.assets.clone(),
        })
    }

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

/// Back-compat shape used before profile inheritance existed.
impl VersionManifest {
    pub fn resolve_classpath(
        &self,
        libraries_dir: &Path,
        client_jar: &Path,
        platform: &PlatformInfo,
    ) -> Vec<PathBuf> {
        MergedProfile::merge(std::slice::from_ref(self))
            .map(|m| m.resolve_classpath(libraries_dir, client_jar, platform))
            .unwrap_or_default()
    }
}
