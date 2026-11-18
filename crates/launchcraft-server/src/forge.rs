//! Forge / NeoForge runtime staging, without a JDK.
//!
//! Phase 11 originally assumed the server would have to *run* a Forge
//! installer jar (`java -jar forge-installer.jar --installClient`) to get a
//! launch profile, which would mean shipping a JDK in the image. Inspecting
//! the real artifacts shows that is not needed: the installer is a zip whose
//! `version.json` **is** a Mojang-format launch profile — `mainClass`, the
//! full `libraries` list with download URLs, and `inheritsFrom` the vanilla
//! version — for Forge 1.12.2 all the way up to current NeoForge. The
//! installer's `processors`/binpatch steps exist to build a *server* jar;
//! a client profile needs nothing but the profile and the jars it names.
//!
//! So staging a Forge runtime is: fetch the installer, read `version.json`
//! out of it, and hand it to
//! [`launchcraft_runtime::meta::MinecraftClient::install_overlay_profile`],
//! which merges the vanilla chain, downloads the client jar, asset index and
//! libraries, and installs everything under `versions/<mc>-<kind>-<ver>/`.
//!
//! What is deliberately *not* taken from the installer:
//!
//! * `install_profile.json`'s `libraries` — those are the installer's own
//!   dependencies (lzma, javaxdelta, opencsv, …), not game libraries.
//! * `data/`, `maven/`, `*.natives` payloads and the embedded launcher
//!   classes: the profile's library list already points at the maven copies.
//! * The universal jar's *contents* are never unpacked; it is downloaded as a
//!   normal library and hashed like everything else.

use std::io::Read as _;
use std::sync::Arc;

use axum::http::StatusCode;
use launchcraft_runtime::meta::{MinecraftClient, OverlayInstall, VersionListFile};

use crate::runtime_dist::RuntimeKey;
use crate::{ApiError, AppState};

/// Installers are small (7 MB for current NeoForge, 6 MB for Forge 1.20.1) —
/// this is the ceiling, not the expected size.
const MAX_INSTALLER_BYTES: u64 = 128 * 1024 * 1024;

/// The profile file inside every Forge/NeoForge installer since 1.6-era.
const PROFILE_ENTRY: &str = "version.json";

/// Which Forge-family loader a runtime key asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    Forge,
    NeoForge,
}

impl Family {
    pub fn id(self) -> &'static str {
        match self {
            Family::Forge => "forge",
            Family::NeoForge => "neoforge",
        }
    }

    pub fn parse(kind: &str) -> Option<Self> {
        match kind {
            "forge" => Some(Family::Forge),
            "neoforge" => Some(Family::NeoForge),
            _ => None,
        }
    }

    /// Maven coordinates of the installer jar for one release.
    ///
    /// Forge builds are keyed by `<mc>-<forge>` (a Forge version only ever
    /// targets one Minecraft version); NeoForge dropped the MC prefix and is
    /// keyed by its own version, which encodes the MC release (`21.1.x` is
    /// Minecraft 1.21.1).
    pub fn installer_url(self, maven_base: &str, key: &RuntimeKey) -> String {
        let base = maven_base.trim_end_matches('/');
        match self {
            Family::Forge => {
                let coord = format!(
                    "{}-{}",
                    key.mc_version,
                    key.loader_version.clone().unwrap_or_default()
                );
                format!("{base}/net/minecraftforge/forge/{coord}/forge-{coord}-installer.jar")
            }
            Family::NeoForge => {
                let version = key.loader_version.clone().unwrap_or_default();
                format!("{base}/net/neoforged/neoforge/{version}/neoforge-{version}-installer.jar")
            }
        }
    }

    pub fn maven_base(self, state: &AppState) -> &str {
        match self {
            Family::Forge => &state.forge_base,
            Family::NeoForge => &state.neoforge_base,
        }
    }
}

/// Is this key a Forge-family one?
pub fn family_of(key: &RuntimeKey) -> Option<Family> {
    key.loader.as_deref().and_then(Family::parse)
}

/// Fetch the installer and install the launch profile it carries. Returns the
/// install report, whose `version` is the `versions/<id>` entry that landed.
pub async fn stage(
    state: &Arc<AppState>,
    client: &MinecraftClient,
    list: &VersionListFile,
    key: &RuntimeKey,
    with_assets: bool,
) -> Result<launchcraft_runtime::meta::InstallReport, ApiError> {
    let family = family_of(key).ok_or_else(|| {
        ApiError::bad_request(format!(
            "{} is not a Forge-family loader",
            key.loader.clone().unwrap_or_default()
        ))
    })?;
    let loader_version = key.loader_version.clone().unwrap_or_default();
    if loader_version.is_empty() {
        return Err(ApiError::bad_request(format!(
            "a {loader} runtime needs an explicit loader version (the installer is \
             addressed by it)",
            loader = family.id()
        )));
    }

    let url = family.installer_url(family.maven_base(state), key);
    let jar = fetch_bounded(state, &url).await?;
    let profile = extract_profile(&jar).map_err(|e| {
        ApiError::new(
            StatusCode::BAD_GATEWAY,
            format!("could not read {PROFILE_ENTRY} from {url}: {e}"),
        )
    })?;

    // The profile must belong to the Minecraft version we were asked for.
    // NeoForge ids alone do not say so, and a mistyped version would otherwise
    // stage a runtime under the wrong key.
    let inherits = serde_json::from_slice::<serde_json::Value>(&profile)
        .ok()
        .and_then(|v| {
            v.get("inheritsFrom")
                .and_then(|s| s.as_str())
                .map(str::to_string)
        });
    if let Some(parent) = &inherits {
        if parent != &key.mc_version {
            return Err(ApiError::bad_request(format!(
                "{url} is a profile for Minecraft {parent}, not {}",
                key.mc_version
            )));
        }
    }

    let report = client
        .install_overlay_profile(OverlayInstall {
            install_id: format!("{}-{}-{}", key.mc_version, family.id(), loader_version),
            base_version_id: &key.mc_version,
            profile_json: &profile,
            list,
            with_assets,
            progress: None,
            maven_base: Some(family.maven_base(state)),
        })
        .await
        .map_err(|e| {
            ApiError::new(
                StatusCode::BAD_GATEWAY,
                format!("{} runtime install: {e}", family.id()),
            )
        })?;

    tracing::info!(
        "staged {} runtime for {} (installer {} bytes, {} file(s) written, {} skipped)",
        family.id(),
        key.id(),
        jar.len(),
        report.files_written,
        report.files_skipped
    );
    // The profile's own id was rewritten to `<mc>-<kind>-<version>` by the
    // installer pipeline, so `report.version` is the launch id packs expect.
    Ok(report)
}

/// Pull a bounded number of bytes from the loader maven.
async fn fetch_bounded(state: &Arc<AppState>, url: &str) -> Result<Vec<u8>, ApiError> {
    let mut res = state
        .http
        .get(url)
        .send()
        .await
        .map_err(|e| ApiError::new(StatusCode::BAD_GATEWAY, format!("download failed: {e}")))?;
    if !res.status().is_success() {
        return Err(ApiError::new(
            res.status(),
            format!(
                "{} ({}) — check the Minecraft version and loader version pair",
                res.status(),
                url
            ),
        ));
    }
    if let Some(len) = res
        .headers()
        .get(axum::http::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
    {
        if len > MAX_INSTALLER_BYTES {
            return Err(ApiError::new(
                StatusCode::BAD_GATEWAY,
                format!(
                    "installer is {} bytes, over the {MAX_INSTALLER_BYTES} byte cap",
                    len
                ),
            ));
        }
    }
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = res
        .chunk()
        .await
        .map_err(|e| ApiError::new(StatusCode::BAD_GATEWAY, format!("stream failed: {e}")))?
    {
        if buf.len() as u64 + chunk.len() as u64 > MAX_INSTALLER_BYTES {
            return Err(ApiError::new(
                StatusCode::BAD_GATEWAY,
                format!("installer exceeded the {MAX_INSTALLER_BYTES} byte cap"),
            ));
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

/// Read `version.json` out of an installer jar.
///
/// The archive is untrusted input, so entries are matched by exact name and
/// nothing is ever written to disk from it — the profile bytes are what the
/// staging tree gets, everything else is downloaded through the normal
/// hash-verified library pipeline.
pub fn extract_profile(jar: &[u8]) -> Result<Vec<u8>, String> {
    let mut archive =
        zip::ZipArchive::new(std::io::Cursor::new(jar)).map_err(|e| format!("not a zip: {e}"))?;
    let mut file = archive
        .by_name(PROFILE_ENTRY)
        .map_err(|_| format!("no {PROFILE_ENTRY} in the installer"))?;
    let mut out = Vec::with_capacity(file.size() as usize);
    file.read_to_end(&mut out)
        .map_err(|e| format!("could not read {PROFILE_ENTRY}: {e}"))?;
    // Parse it here so a truncated or non-json entry fails at the right place.
    let value: serde_json::Value =
        serde_json::from_slice(&out).map_err(|e| format!("{PROFILE_ENTRY} is not json: {e}"))?;
    if value.get("mainClass").and_then(|v| v.as_str()).is_none() {
        return Err(format!("{PROFILE_ENTRY} has no mainClass"));
    }
    if value.get("libraries").and_then(|v| v.as_array()).is_none() {
        return Err(format!("{PROFILE_ENTRY} has no libraries list"));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime_dist::RuntimeKey;

    fn key(mc: &str, loader: &str, version: &str) -> RuntimeKey {
        RuntimeKey {
            mc_version: mc.into(),
            loader: Some(loader.into()),
            loader_version: Some(version.into()),
        }
    }

    #[test]
    fn installer_urls_follow_each_families_maven_layout() {
        assert_eq!(
            Family::Forge.installer_url("https://maven.minecraftforge.net", &key("1.20.1", "forge", "47.2.0")),
            "https://maven.minecraftforge.net/net/minecraftforge/forge/1.20.1-47.2.0/forge-1.20.1-47.2.0-installer.jar"
        );
        assert_eq!(
            Family::NeoForge.installer_url("https://maven.neoforged.net/releases/", &key("1.21.1", "neoforge", "21.1.267")),
            "https://maven.neoforged.net/releases/net/neoforged/neoforge/21.1.267/neoforge-21.1.267-installer.jar"
        );
        // A trailing slash in the configured base never doubles up.
        assert_eq!(
            Family::Forge.installer_url("http://h:1/maven//", &key("1.20.1", "forge", "47.2.0")),
            "http://h:1/maven/net/minecraftforge/forge/1.20.1-47.2.0/forge-1.20.1-47.2.0-installer.jar"
        );
    }

    #[test]
    fn only_forge_and_neoforge_are_this_modules_business() {
        assert_eq!(
            family_of(&key("1.20.1", "forge", "47.2.0")),
            Some(Family::Forge)
        );
        assert_eq!(
            family_of(&key("1.21.1", "neoforge", "21.1.267")),
            Some(Family::NeoForge)
        );
        assert_eq!(family_of(&key("1.20.1", "fabric", "0.15.11")), None);
        assert_eq!(family_of(&key("1.20.1", "", "1")), None);
    }

    /// A builder so the tests can describe installers the way the maven hosts do.
    fn installer(entries: Vec<(&str, &str)>) -> Vec<u8> {
        let buf = std::io::Cursor::new(Vec::new());
        let mut zip = zip::ZipWriter::new(buf);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        for (name, body) in entries {
            zip.start_file(name, options).unwrap();
            std::io::Write::write_all(&mut zip, body.as_bytes()).unwrap();
        }
        zip.finish().unwrap().into_inner()
    }

    fn profile(mc: &str) -> String {
        format!(
            r#"{{"id":"{mc}-forge-47.2.0","inheritsFrom":"{mc}","mainClass":"cpw.mods.bootstraplauncher.BootstrapLauncher","libraries":[{{"name":"net.minecraftforge:forge:{mc}-47.2.0","downloads":{{"artifact":{{"path":"net/minecraftforge/forge/{mc}-47.2.0/forge-{mc}-47.2.0.jar","url":""}}}}}}]}}"#
        )
    }

    #[test]
    fn extracts_the_profile_and_rejects_useless_ones() {
        let jar = installer(vec![
            ("META-INF/MANIFEST.MF", "Manifest-Version: 1.0"),
            (
                "install_profile.json",
                r#"{"version":"1.20.1-forge-47.2.0","libraries":[]}"#,
            ),
            ("version.json", &profile("1.20.1")),
            ("data/foo.lzma", "binary junk"),
        ]);
        let got = extract_profile(&jar).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&got).unwrap();
        assert_eq!(
            v["mainClass"],
            "cpw.mods.bootstraplauncher.BootstrapLauncher"
        );
        assert_eq!(v["inheritsFrom"], "1.20.1");
        assert_eq!(v["libraries"].as_array().unwrap().len(), 1);

        // No profile at all (a plain jar, or a truncated download).
        let bare = installer(vec![("install_profile.json", "{}")]);
        assert!(extract_profile(&bare)
            .unwrap_err()
            .contains("no version.json"));
        // Present but unusable: must fail loudly, not install an empty profile.
        let no_main = installer(vec![("version.json", r#"{"id":"x","libraries":[]}"#)]);
        assert!(extract_profile(&no_main)
            .unwrap_err()
            .contains("no mainClass"));
        let no_libs = installer(vec![("version.json", r#"{"id":"x","mainClass":"a.B"}"#)]);
        assert!(extract_profile(&no_libs)
            .unwrap_err()
            .contains("no libraries list"));
        // Not a zip.
        assert!(extract_profile(b"not a zip file").is_err());
    }
}
