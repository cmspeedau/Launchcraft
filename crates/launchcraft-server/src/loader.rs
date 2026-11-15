//! Mod loader data acquisition: turns a pack's loader requirement into the
//! files a launcher needs inside `src/loader/`, so synced instances can
//! actually start the game.
//!
//! Fabric/Quilt ship a loader *profile* (a version-json fragment plus the
//! libraries it names) from their meta APIs; Forge/NeoForge ship a single
//! installer jar on their Maven. Everything lands under `packs/<id>/src/`
//! `loader/`, which the builder packs like any other file (it only skips
//! `pack.json`), so client syncs carry the loader automatically.

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

use axum::http::StatusCode;
use serde_json::Value;

use crate::{ApiError, AppState};

/// A parsed loader requirement: kind + version from the pack plan
/// (`"fabric-0.15.11"` style) plus the Minecraft version it targets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoaderSpec {
    /// `"fabric"`, `"quilt"`, `"forge"` or `"neoforge"`.
    pub kind: String,
    /// Loader version, e.g. `"0.15.11"` (forge: `"47.2.0"`).
    pub version: String,
    /// Minecraft version, e.g. `"1.20.1"`.
    pub game_version: String,
}

impl LoaderSpec {
    /// Parse a plan loader string against a game version. `None` means the
    /// plan names nothing fetchable (empty, unknown kind, or tokens that
    /// must not reach a URL); callers turn that into a warning.
    pub fn detect(raw: &str, game_version: &str) -> Option<LoaderSpec> {
        let game_version = game_version.trim();
        if game_version.is_empty() || !safe_token(game_version) {
            return None;
        }
        let (kind, version) = raw.trim().split_once('-')?;
        let kind = kind.to_ascii_lowercase();
        if !matches!(kind.as_str(), "fabric" | "quilt" | "forge" | "neoforge") {
            return None;
        }
        if !safe_token(version) {
            return None;
        }
        Some(LoaderSpec {
            kind,
            version: version.to_string(),
            game_version: game_version.to_string(),
        })
    }
}

/// A token safe to embed in URLs and filesystem paths: no separators at all.
fn safe_token(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'+'))
}

/// Fetch the loader data for `spec` into `src/loader/`, replacing whatever
/// was there. Returns the number of files written (always ≥ 1).
pub async fn fetch(state: &AppState, src: &Path, spec: &LoaderSpec) -> Result<usize, ApiError> {
    let dir = src.join("loader");
    if dir.exists() {
        std::fs::remove_dir_all(&dir).map_err(|e| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("cannot reset {}: {e}", dir.display()),
            )
        })?;
    }
    std::fs::create_dir_all(&dir)?;
    match spec.kind.as_str() {
        "fabric" => maven_meta(state, &state.fabric_base, &dir, spec).await,
        "quilt" => maven_meta(state, &state.quilt_base, &dir, spec).await,
        "forge" => {
            let mv = format!("{}-{}", spec.game_version, spec.version);
            let name = format!("forge-{mv}-installer.jar");
            let url = format!("{}/net/minecraftforge/forge/{mv}/{name}", state.forge_base);
            installer(state, &url, &dir.join(&name)).await
        }
        // "neoforge" — Kind::detect already restricted the set.
        _ => {
            let name = format!("neoforge-{}-installer.jar", spec.version);
            let url = format!(
                "{}/net/neoforged/neoforge/{}/{}",
                state.neoforge_base, spec.version, name
            );
            installer(state, &url, &dir.join(&name)).await
        }
    }
}

/// Fabric/Quilt: the loader `profile.json` plus every library jar it
/// references under `libraries/` (maven-style paths taken from the
/// download URLs, so no path data is invented server-side).
async fn maven_meta(
    state: &AppState,
    base: &str,
    dir: &Path,
    spec: &LoaderSpec,
) -> Result<usize, ApiError> {
    let url = format!(
        "{}/versions/loader/{}/{}/profile/json",
        base, spec.game_version, spec.version
    );
    let body = crate::modpacks::fetch_bytes(state, &url).await?;
    let profile: Value = serde_json::from_slice(&body).map_err(|e| {
        ApiError::new(
            StatusCode::BAD_GATEWAY,
            format!("bad loader profile from {url}: {e}"),
        )
    })?;
    std::fs::write(dir.join("profile.json"), &body)?;
    let mut written = 1;
    let mut seen: HashSet<String> = HashSet::new();
    let libs = profile
        .get("libraries")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for lib in &libs {
        // Candidate jar URLs across the metadata dialects: launchermeta
        // `downloads.{common,client,server,artifact}.url`, and the fabric
        // meta style of a repo `url` plus a `maven` relative path or plain
        // maven coordinates in `name`.
        let mut urls: Vec<String> = Vec::new();
        for side in ["common", "client", "server", "artifact"] {
            if let Some(u) = lib
                .pointer(&format!("/downloads/{side}/url"))
                .and_then(Value::as_str)
                .filter(|u| !u.is_empty())
            {
                urls.push(u.to_string());
            }
        }
        if urls.is_empty() {
            if let Some(repo) = lib
                .get("url")
                .and_then(Value::as_str)
                .filter(|u| !u.is_empty())
            {
                let rel = lib
                    .get("maven")
                    .and_then(Value::as_str)
                    .filter(|m| !m.is_empty())
                    .map(str::to_string)
                    .or_else(|| {
                        lib.get("name")
                            .and_then(Value::as_str)
                            .and_then(coord_path)
                            .map(|p| p.to_string_lossy().into_owned())
                    });
                if let Some(rel) = rel {
                    urls.push(format!(
                        "{}/{}",
                        repo.trim_end_matches('/'),
                        rel.trim_start_matches('/')
                    ));
                }
            }
        }
        for url in urls {
            if !seen.insert(url.clone()) {
                continue; // maven coordinates repeat across sides
            }
            let rel = maven_rel(&url).ok_or_else(|| {
                ApiError::new(
                    StatusCode::BAD_GATEWAY,
                    format!("loader library has no usable download path: {url}"),
                )
            })?;
            let dest = dir.join("libraries").join(&rel);
            std::fs::create_dir_all(dest.parent().ok_or_else(|| {
                ApiError::new(
                    StatusCode::BAD_GATEWAY,
                    format!("bad loader library path: {url}"),
                )
            })?)?;
            let bytes = crate::modpacks::fetch_bytes(state, &url).await?;
            std::fs::write(&dest, &bytes)?;
            written += 1;
        }
    }
    Ok(written)
}

/// Forge/NeoForge: fetch the installer jar (launchers know how to run its
/// embedded server installer; the client host extracts from it too).
async fn installer(state: &AppState, url: &str, dest: &Path) -> Result<usize, ApiError> {
    let bytes = crate::modpacks::fetch_bytes(state, url).await?;
    std::fs::write(dest, bytes)?;
    Ok(1)
}

/// One filesystem segment: readable chars only, and never just dots.
fn safe_seg(s: &str) -> bool {
    !s.is_empty()
        && !s.bytes().all(|b| b == b'.')
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'+' | b'-'))
}

/// Maven coordinates `group:artifact:version[:classifier][@ext]` → the
/// repository-relative jar path (`group/artifact/version/artifact-version.jar`).
fn coord_path(coord: &str) -> Option<PathBuf> {
    let (coord, ext) = match coord.split_once('@') {
        Some((c, e)) if safe_seg(e) => (c, e),
        _ => (coord, "jar"),
    };
    let parts: Vec<&str> = coord.split(':').collect();
    if parts.len() < 3 || !parts[..3].iter().all(|p| !p.is_empty()) {
        return None;
    }
    let group = parts[0]
        .split('.')
        .map(|seg| safe_seg(seg).then_some(()))
        .collect::<Option<Vec<_>>>()?;
    if group.is_empty() || !safe_seg(parts[1]) || !safe_seg(parts[2]) {
        return None;
    }
    let classifier = match parts.get(3) {
        Some(c) if safe_seg(c) => format!("-{c}"),
        Some(_) => return None,
        None => String::new(),
    };
    let mut out = PathBuf::from(parts[0].replace('.', "/"));
    out.push(parts[1]);
    out.push(parts[2]);
    out.push(format!("{}-{}{}.{}", parts[1], parts[2], classifier, ext));
    Some(out)
}

/// A download URL's host+path as a repository-relative path, so jars keep
/// their upstream coordinates locally (host-prefixed, like a URL mirror).
/// Anything that is not a plain chain of normal path segments (dot-dot,
/// empty, weird) is refused.
fn maven_rel(url: &str) -> Option<PathBuf> {
    let rest = url.split_once("://")?.1;
    let (host, path) = rest.split_once('/')?;
    // The host lands on disk as a directory: same-characters-safe, and it
    // must say something ("..", empty → refuse, would traverse).
    if host.is_empty()
        || !host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b':' | b'[' | b']'))
        || !host.bytes().any(|b| b.is_ascii_alphanumeric())
    {
        return None;
    }
    let path = path.split('?').next().unwrap_or(path);
    let mut out = PathBuf::from(host);
    for comp in Path::new(path).components() {
        match comp {
            Component::Normal(part) => out.push(part),
            Component::CurDir => {}
            _ => return None,
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_parses_known_loaders() {
        let s = LoaderSpec::detect("fabric-0.15.11", "1.20.1").unwrap();
        assert_eq!(
            s,
            LoaderSpec {
                kind: "fabric".into(),
                version: "0.15.11".into(),
                game_version: "1.20.1".into(),
            }
        );
        // Case-insensitive kind, forge keeps its own version token.
        let f = LoaderSpec::detect("Forge-47.2.0", "1.20.1").unwrap();
        assert_eq!((f.kind.as_str(), f.version.as_str()), ("forge", "47.2.0"));
        assert!(LoaderSpec::detect("neoforge-21.0.37", "1.21").is_some());
        assert!(LoaderSpec::detect("quilt-0.23.1", "1.20.4").is_some());
    }

    #[test]
    fn detect_rejects_unfetchable_requirements() {
        assert!(LoaderSpec::detect("", "1.20.1").is_none());
        assert!(LoaderSpec::detect("liteloader-1.0", "1.20.1").is_none());
        assert!(LoaderSpec::detect("optifine", "1.20.1").is_none()); // no version part
        assert!(LoaderSpec::detect("fabric-0.15.11", "").is_none()); // no game version
                                                                     // Path/traversal-ish tokens never reach URLs or paths.
        assert!(LoaderSpec::detect("fabric-../../x", "1.20.1").is_none());
        assert!(LoaderSpec::detect("fabric-a b", "1.20.1").is_none());
        assert!(LoaderSpec::detect("fabric-0.15", "1.20.1 /x").is_none());
    }

    #[test]
    fn coord_path_maps_maven_coordinates() {
        assert_eq!(
            coord_path("net.fabricmc:fabric-loader:0.14.13"),
            Some(PathBuf::from(
                "net/fabricmc/fabric-loader/0.14.13/fabric-loader-0.14.13.jar"
            ))
        );
        assert_eq!(
            coord_path("net.fabricmc:tiny-mappings-parser:0.3.0+build.17"),
            Some(PathBuf::from(
                "net/fabricmc/tiny-mappings-parser/0.3.0+build.17/tiny-mappings-parser-0.3.0+build.17.jar"
            ))
        );
        assert_eq!(
            coord_path("g.a:art:1.0:sources@zip"),
            Some(PathBuf::from("g/a/art/1.0/art-1.0-sources.zip"))
        );
        assert!(coord_path("net.fabricmc:fabric-loader").is_none()); // no version
        assert!(coord_path("g.a:art:../../x").is_none()); // traversal version
        assert!(coord_path("g..a:art:1.0").is_none()); // empty group segment
        assert!(coord_path("g.a:art:1.0:bad classifier").is_none());
    }

    #[test]
    fn maven_rel_maps_and_refuses() {
        assert_eq!(
            maven_rel("https://maven.fabricmc.net/net/fabricmc/fabric-loader/0.15.11/fabric-loader-0.15.11.jar"),
            Some(PathBuf::from(
                "maven.fabricmc.net/net/fabricmc/fabric-loader/0.15.11/fabric-loader-0.15.11.jar"
            ))
        );
        assert_eq!(
            maven_rel("http://host/a/b.jar?v=1"),
            Some(PathBuf::from("host/a/b.jar"))
        );
        assert!(maven_rel("ftp.example/x.jar").is_none()); // no scheme separator
        assert!(maven_rel("https://host").is_none()); // no path
        assert!(maven_rel("https://../x.jar").is_none()); // dot-only host
        assert!(maven_rel("https://host/../escape.jar").is_none());
        assert!(maven_rel("https://host/a/../../escape.jar").is_none());
    }
}
