//! Server-side Java runtimes (the JDKs **this** server manages, not a
//! launcher install on someone's desktop).
//!
//! A self-hosted pack server does not serve one Minecraft version: a `1.7.10`
//! pack needs Java 8, `1.12.2` Java 8, `1.17` Java 16, `1.18`–`1.20.4` Java 17,
//! `1.20.5+` Java 21 — and all of those can be live on one instance at the
//! same time. So Java is keyed by **major version**, one runtime per major,
//! shared by every pack and runtime that asks for it, rather than baked into
//! the container image as a single JDK.
//!
//! Resolution order for a major:
//!
//! 1. `LAUNCHCRAFT_JAVA_<MAJOR>` — an explicit path, for air-gapped hosts.
//! 2. `<data>/jres/java-<major>` — already provisioned by this server.
//! 3. `LAUNCHCRAFT_JAVA` / `JAVA_HOME` / `java` on `PATH` — a host JDK, used
//!    only when `java -version` says it really is that major.
//! 4. Adoptium (Temurin), downloaded, sha256-verified and extracted into the
//!    managed directory, so it survives container restarts on the data volume.
//!
//! Java 7 and 6 (Minecraft ≤ 1.5.x) are not downloadable from Adoptium, so
//! those majors only resolve through an explicit `LAUNCHCRAFT_JAVA_7` style
//! override; the console says so instead of failing mysteriously.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use launchcraft_runtime::java::{AdoptiumClient, JavaRuntimeManager};
use launchcraft_runtime::platform::PlatformInfo;

use crate::runtime_dist::RuntimeRegistry;
use crate::store::write_atomic;
use crate::{ApiError, AppState};

/// Where this server keeps the JDKs it provisions.
pub struct JreStore {
    pub root: PathBuf,
}

impl JreStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    fn registry_path(&self) -> PathBuf {
        self.root.join("registry.json")
    }

    pub fn managed_dir(&self, major: u32) -> PathBuf {
        self.root.join(format!("java-{major}"))
    }

    pub fn ensure_layout(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.root)?;
        Ok(())
    }

    pub fn load(&self) -> Result<JreRegistry, ApiError> {
        let path = self.registry_path();
        if !path.is_file() {
            return Ok(JreRegistry::default());
        }
        let text = std::fs::read_to_string(&path).map_err(|e| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("could not read {}: {e}", path.display()),
            )
        })?;
        serde_json::from_str(&text).map_err(|e| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("could not parse {}: {e}", path.display()),
            )
        })
    }

    pub fn save(&self, registry: &JreRegistry) -> Result<(), ApiError> {
        self.ensure_layout()?;
        let body = serde_json::to_vec_pretty(registry)?;
        write_atomic(&self.registry_path(), &body)?;
        Ok(())
    }

    /// What `java -version` reports for a binary, e.g.
    /// `openjdk version "1.8.0_422"` → `Some((8, "1.8.0_422"))`.
    fn probe(path: &Path) -> Option<(u32, String)> {
        let out = Command::new(path)
            .arg("-version")
            .stderr(std::process::Stdio::piped())
            .output()
            .ok()?;
        // Modern JDKs print to stderr too; accept whichever stream has it.
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stderr),
            String::from_utf8_lossy(&out.stdout)
        );
        let raw = text
            .lines()
            .find_map(|line| line.split('"').nth(1))
            .unwrap_or_default()
            .to_string();
        let major = parse_java_version(&raw)?;
        Some((major, raw))
    }
}

/// Majors the console offers to provision. Everything from Java 8 up is
/// downloadable from Adoptium; Java 7 and 6 (Minecraft ≤ 1.5.x) stay listed so
/// the UI can explain they need a `LAUNCHCRAFT_JAVA_<n>` path instead.
pub const KNOWN_MAJORS: [u32; 8] = [6, 7, 8, 11, 16, 17, 21, 24];

/// `1.8.0_422`, `8.0.402`, `17.0.2`, `21` → the major (`8`, `17`, `21`).
pub fn parse_java_version(raw: &str) -> Option<u32> {
    let first = raw.split(|c: char| !c.is_ascii_digit()).next()?;
    let first: u32 = first.parse().ok()?;
    // Java 8 and older report `1.8.0_422`: the real major is the second part.
    if first == 1 {
        let rest = raw.split_once('.')?.1;
        let second: u32 = rest
            .split(|c: char| !c.is_ascii_digit())
            .next()?
            .parse()
            .ok()?;
        return (2..=40).contains(&second).then_some(second);
    }
    (2..=40).contains(&first).then_some(first)
}

// ------------------------------------------------------------- requirement

/// Which Java major a Minecraft version needs. `profile_major` is the
/// `javaVersion.majorVersion` the version (or loader) profile declares and
/// wins whenever it is present; the table below is the fallback for the old
/// profiles that carry no `javaVersion` at all.
pub fn required_major(mc_version: &str, profile_major: Option<u32>) -> u32 {
    if let Some(major) = profile_major.filter(|m| (2..=40).contains(m)) {
        return major;
    }
    let version = mc_version.trim();
    if let Some(code) = snapshot_year(version) {
        // Snapshots track the release they precede; the profile normally says
        // so, this is the fallback for pre-1.12 style names.
        return match code {
            y if y >= 24 => 21,
            21..=23 => 17,
            _ => 8,
        };
    }
    match release_parts(version) {
        // 1.20.5 and newer require Java 21; 1.18–1.20.4 Java 17; 1.17 Java 16.
        Some((1, minor, _)) if minor >= 21 => 21,
        Some((1, 20, patch)) if patch >= 5 => 21,
        Some((1, 18..=20, _)) => 17,
        Some((1, 17, _)) => 16,
        // Everything down to beta-era packs runs on Java 8 (1.7.10, 1.10.2,
        // 1.12.2 included). Minecraft ≤ 1.5.2 wants Java 6/7: not available
        // from Adoptium, so those need a LAUNCHCRAFT_JAVA_<n> override.
        Some((1, _, _)) => 8,
        _ => 17,
    }
}

/// `1.20.1` / `1.7.10` / `1.8.9` → `(1, minor, patch)`.
fn release_parts(version: &str) -> Option<(u32, u32, u32)> {
    let mut parts = version.split('.');
    let major: u32 = parts.next()?.parse().ok()?;
    let minor: u32 = parts.next()?.parse().ok()?;
    let patch = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    Some((major, minor, patch))
}

/// `24w21a`-style snapshot names → the two-digit year, else `None`.
fn snapshot_year(version: &str) -> Option<u32> {
    let (year, rest) = version.split_once('w')?;
    let year: u32 = year.parse().ok()?;
    (!rest.is_empty()
        && rest
            .chars()
            .all(|c| c.is_ascii_digit() || c.is_ascii_alphabetic()))
    .then_some(year)
}

// ---------------------------------------------------------------- records

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JreSource {
    /// Found through an explicit `LAUNCHCRAFT_JAVA*` override.
    Env,
    /// A JDK already on the host (`JAVA_HOME` / `java` on PATH).
    System,
    /// Downloaded from Adoptium into `<data>/jres/java-<major>`.
    Managed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JreRecord {
    pub major: u32,
    pub path: String,
    pub source: JreSource,
    #[serde(default)]
    pub status: JreStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub updated_at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum JreStatus {
    Installing,
    #[default]
    Ready,
    Error,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct JreRegistry {
    #[serde(default)]
    pub jres: Vec<JreRecord>,
}

impl JreRegistry {
    pub fn find(&self, major: u32) -> Option<&JreRecord> {
        self.jres
            .iter()
            .filter(|r| r.major == major && r.status == JreStatus::Ready)
            // Prefer an explicit override, then a host JDK, then ours.
            .max_by_key(|r| match r.source {
                JreSource::Env => 3,
                JreSource::System => 2,
                JreSource::Managed => 1,
            })
    }

    pub fn upsert(&mut self, record: JreRecord) {
        match self
            .jres
            .iter_mut()
            .find(|r| r.major == record.major && r.source == record.source)
        {
            Some(existing) => *existing = record,
            None => self.jres.push(record),
        }
        self.jres.sort_by_key(|r| r.major);
    }

    pub fn remove_major(&mut self, major: u32) -> Vec<JreRecord> {
        let taken: Vec<JreRecord> = self
            .jres
            .iter()
            .filter(|r| r.major == major)
            .cloned()
            .collect();
        self.jres.retain(|r| r.major != major);
        taken
    }
}

/// The manager that does the actual Adoptium fetch, pointed at this server's
/// managed directory and (for tests and mirrors) a configurable API base.
fn manager(state: &AppState, root: &Path) -> JavaRuntimeManager {
    JavaRuntimeManager::with_adoptium(
        root,
        PlatformInfo::current(),
        AdoptiumClient::with_base_url(&state.adoptium_url),
    )
}

/// The JDK home a `…/bin/java` belongs to. The path is canonicalised first,
/// because host JREs are usually symlinks (`/usr/bin/java` →
/// `/etc/alternatives/java` → `/usr/lib/jvm/java-17-openjdk-amd64/bin/java`):
/// without that, the "parent of bin" would be `/usr` and the reported size
/// would be the whole operating system.
///
/// Only directories that actually look like a JDK are returned — the marker
/// files every OpenJDK/Temurin build ships — so a stray script on `PATH`
/// never turns into a meaningless number.
fn jdk_home(java_path: &Path) -> Option<PathBuf> {
    let resolved = std::fs::canonicalize(java_path).unwrap_or_else(|_| java_path.to_path_buf());
    let home = resolved.parent()?.parent()?;
    let looks_like_jdk = home.join("release").is_file()
        || home.join("lib").join("modules").is_file()
        || home.join("jre").join("release").is_file();
    looks_like_jdk.then(|| home.to_path_buf())
}

/// Sum the size of a directory tree (best effort, for the console).
fn dir_size(path: &Path) -> u64 {
    walkdir::WalkDir::new(path)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| e.metadata().ok().map(|m| m.len()))
        .sum()
}

/// On-disk footprint of a JDK, or 0 when its home directory cannot be
/// identified (a wrapper script, an unusual layout).
fn jdk_size(java_path: &Path) -> u64 {
    jdk_home(java_path).map(|home| dir_size(&home)).unwrap_or(0)
}

/// First `java` executable on a PATH-like string. Container images install a
/// JRE at `/usr/bin/java` without exporting `JAVA_HOME`, so PATH is worth
/// checking before spending 180 MB on a download the host already has.
fn java_on_path(path_var: &str) -> Option<PathBuf> {
    let name = if cfg!(windows) { "java.exe" } else { "java" };
    let separator = if cfg!(windows) { ';' } else { ':' };
    path_var
        .split(separator)
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// JDKs already on the host, in precedence order: `java` on PATH, then
/// `JAVA_HOME`. Container images install a JRE at `/usr/bin/java` without
/// exporting `JAVA_HOME`, so PATH is checked first — before spending 180 MB on
/// a download the host already has.
fn system_java_candidates(path_var: &str, java_home: Option<&str>) -> Vec<PathBuf> {
    let exe = if cfg!(windows) { "java.exe" } else { "java" };
    let mut out: Vec<PathBuf> = Vec::new();
    if let Some(from_path) = java_on_path(path_var) {
        out.push(from_path);
    }
    if let Some(home) = java_home.filter(|h| !h.trim().is_empty()) {
        let bin = PathBuf::from(home).join("bin").join(exe);
        if !out.contains(&bin) {
            out.push(bin);
        }
    }
    out
}

/// Record the JDKs we can see — explicit `LAUNCHCRAFT_JAVA[_<MAJOR>]` paths,
/// and (unless the operator pinned the pool) whatever the host already has —
/// so the console shows the whole picture and probes happen once.
fn discover(state: &AppState, registry: &mut JreRegistry) {
    let now = chrono::Utc::now().to_rfc3339();
    let mut candidates: Vec<(Option<u32>, PathBuf, JreSource)> = Vec::new();
    if state.jres_host_java {
        candidates.extend(
            system_java_candidates(
                &std::env::var("PATH").unwrap_or_default(),
                std::env::var("JAVA_HOME").ok().as_deref(),
            )
            .into_iter()
            .map(|path| (None, path, JreSource::System)),
        );
    }
    for (key, value) in std::env::vars() {
        let key = key.to_ascii_uppercase();
        if value.trim().is_empty() {
            continue;
        }
        if let Some(rest) = key.strip_prefix("LAUNCHCRAFT_JAVA_") {
            if let Ok(major) = rest.parse::<u32>() {
                candidates.push((Some(major), PathBuf::from(value), JreSource::Env));
            }
        } else if key == "LAUNCHCRAFT_JAVA" {
            candidates.push((None, PathBuf::from(value), JreSource::Env));
        }
    }
    let mut seen: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    for (declared, path, source) in candidates {
        // JAVA_HOME and PATH often point at the same binary.
        if !path.is_file() || !seen.insert(path.clone()) {
            continue;
        }
        let probed = JreStore::probe(&path);
        let major = match (declared, probed.as_ref()) {
            (Some(major), _) => major,
            (None, Some((probed, _))) => *probed,
            (None, None) => continue, // unusable path and unknown version
        };
        if registry
            .jres
            .iter()
            .any(|r| r.major == major && r.source == source && r.path == path.display().to_string())
        {
            continue;
        }
        registry.upsert(JreRecord {
            major,
            path: path.display().to_string(),
            source,
            status: JreStatus::Ready,
            version: probed.map(|(_, raw)| raw),
            error: None,
            updated_at: now.clone(),
        });
    }
    // Forget anything whose binary has since disappeared (a pruned volume, a
    // removed override) so a stale record can never claim a Java is available.
    // Failed and in-flight slots stay, so the console can show what happened.
    registry
        .jres
        .retain(|r| r.status != JreStatus::Ready || Path::new(&r.path).is_file());
}

/// Everything the console needs about one major: is it available, from where,
/// and which runtimes require it.
pub fn status_json(registry: &JreRegistry, major: u32, runtimes: &RuntimeRegistry) -> Value {
    let record = registry.find(major);
    let wanted_by: Vec<String> = runtimes
        .runtimes
        .iter()
        .filter(|r| r.required_java == Some(major))
        .map(|r| r.id.clone())
        .collect();
    // Measured from the path on every read: a persisted number can go stale
    // (a JDK upgraded in place, a pruned tree), the directory cannot.
    let size = record.map(|r| jdk_size(Path::new(&r.path))).unwrap_or(0);
    json!({
        "major": major,
        "available": record.is_some(),
        "path": record.map(|r| r.path.clone()),
        "source": record.map(|r| r.source),
        "version": record.and_then(|r| r.version.clone()),
        "size_bytes": size,
        "downloadable": major >= 8,
        "required_by": wanted_by,
    })
}

/// Make a major available: existing record, host JDK, or an Adoptium download
/// into the managed directory. Returns the record that satisfied the request.
pub async fn ensure(state: &Arc<AppState>, major: u32, force: bool) -> Result<JreRecord, ApiError> {
    let _guard = state.jre_lock.lock().await;
    let store = &state.jres;
    let mut registry = store.load()?;
    discover(state, &mut registry);
    if let Some(existing) = registry.find(major) {
        if !force || existing.source != JreSource::Managed {
            let existing = existing.clone();
            store.save(&registry)?;
            return Ok(existing);
        }
    }
    if major < 8 {
        return Err(ApiError::bad_request(format!(
            "Java {major} is not published by Adoptium, so it cannot be downloaded. \
             Point LAUNCHCRAFT_JAVA_{major} at a local JDK/JRE path to use one."
        )));
    }

    // Mark the slot as busy so the console shows what is happening.
    let now = chrono::Utc::now().to_rfc3339();
    registry.upsert(JreRecord {
        major,
        path: store.managed_dir(major).display().to_string(),
        source: JreSource::Managed,
        status: JreStatus::Installing,
        version: None,
        error: None,
        updated_at: now,
    });
    store.save(&registry)?;

    let manager = manager(state, &store.root);
    match manager.ensure_java(major, None).await {
        Ok(path) => {
            let record = JreRecord {
                major,
                path: path.display().to_string(),
                source: JreSource::Managed,
                status: JreStatus::Ready,
                version: JreStore::probe(&path).map(|(_, raw)| raw),
                error: None,
                updated_at: chrono::Utc::now().to_rfc3339(),
            };
            registry.upsert(record.clone());
            store.save(&registry)?;
            tracing::info!(
                "provisioned Java {major} ({} bytes) at {}",
                jdk_size(&path),
                record.path
            );
            Ok(record)
        }
        Err(e) => {
            let message = e.to_string();
            registry.upsert(JreRecord {
                major,
                path: store.managed_dir(major).display().to_string(),
                source: JreSource::Managed,
                status: JreStatus::Error,
                version: None,
                error: Some(message.clone()),
                updated_at: chrono::Utc::now().to_rfc3339(),
            });
            store.save(&registry)?;
            Err(ApiError::new(
                StatusCode::BAD_GATEWAY,
                format!("could not provision Java {major}: {message}"),
            ))
        }
    }
}

// --------------------------------------------------------------- handlers

/// `GET /console/jres` — every Java the server can use, and who needs it.
pub async fn console_list(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    crate::console::auth(&state, &headers)?;
    let registry = {
        let _guard = state.jre_lock.lock().await;
        let mut registry = state.jres.load()?;
        discover(&state, &mut registry);
        state.jres.save(&registry)?;
        registry
    };
    let runtimes = state.runtimes.load_registry()?;

    // The majors an operator can pick from, plus anything a built runtime
    // actually requires, so "Java 8 needed by 1.7.10 packs — not installed"
    // is visible without first trying to provision it.
    let mut majors: Vec<u32> = KNOWN_MAJORS.to_vec();
    majors.extend(registry.jres.iter().map(|r| r.major));
    majors.extend(runtimes.runtimes.iter().filter_map(|r| r.required_java));
    majors.sort_unstable();
    majors.dedup();
    let java: Vec<Value> = majors
        .iter()
        .map(|m| status_json(&registry, *m, &runtimes))
        .collect();

    let overrides: BTreeMap<String, String> = std::env::vars()
        .filter(|(k, _)| {
            let key = k.to_ascii_uppercase();
            key == "LAUNCHCRAFT_JAVA" || key.starts_with("LAUNCHCRAFT_JAVA_")
        })
        .collect();

    Ok(Json(json!({
        "managed_dir": state.jres.root.display().to_string(),
        "java": java,
        "records": registry.jres,
        "overrides": overrides,
        "adoptium_url": state.adoptium_url,
    })))
}

#[derive(Debug, Deserialize)]
pub struct ConsoleJreBody {
    pub major: u32,
    #[serde(default)]
    pub force: Option<bool>,
}

/// `POST /console/jres` — download a JDK major now instead of on first use.
pub async fn console_provision(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(body): Json<ConsoleJreBody>,
) -> Result<Json<Value>, ApiError> {
    crate::console::auth(&state, &headers)?;
    let record = ensure(&state, body.major, body.force.unwrap_or(false)).await?;
    Ok(Json(json!({
        "major": record.major,
        "status": record.status,
        "source": record.source,
        "path": record.path,
        "version": record.version,
    })))
}

/// `DELETE /console/jres/:major?force=1` — forget a Java. Managed trees are
/// deleted; host JDKs are only dropped from the registry. Refused while a
/// built runtime still requires the major.
pub async fn console_delete(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    UrlPath(raw): UrlPath<String>,
    Query(q): Query<BTreeMap<String, String>>,
) -> Result<Json<Value>, ApiError> {
    crate::console::auth(&state, &headers)?;
    let major: u32 = raw
        .parse()
        .map_err(|_| ApiError::bad_request(format!("not a Java major version: {raw}")))?;
    let force = q
        .get("force")
        .map(|v| v == "1" || v == "true")
        .unwrap_or(false);

    let _guard = state.jre_lock.lock().await;
    let runtimes = state.runtimes.load_registry()?;
    let wanted_by: Vec<String> = runtimes
        .runtimes
        .iter()
        .filter(|r| r.required_java == Some(major))
        .map(|r| r.id.clone())
        .collect();
    if !wanted_by.is_empty() && !force {
        return Err(ApiError::bad_request(format!(
            "Java {major} is required by runtime(s) {}; pass force=1 to remove it anyway",
            wanted_by.join(", ")
        )));
    }

    let mut registry = state.jres.load()?;
    let removed = registry.remove_major(major);
    if removed.is_empty() {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            format!("no Java {major} registered"),
        ));
    }
    let mut deleted_dir = false;
    if removed.iter().any(|r| r.source == JreSource::Managed) {
        let dir = state.jres.managed_dir(major);
        if dir.is_dir() {
            std::fs::remove_dir_all(&dir)?;
            deleted_dir = true;
        }
    }
    state.jres.save(&registry)?;
    drop(_guard);

    tracing::info!("removed Java {major} from the runtime pool (deleted tree: {deleted_dir})");
    Ok(Json(json!({
        "removed_major": major,
        "removed": removed.iter().map(|r| r.source).collect::<Vec<_>>(),
        "deleted_tree": deleted_dir,
        "was_required_by": wanted_by,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn java_version_strings_collapse_to_a_major() {
        assert_eq!(parse_java_version("1.8.0_422"), Some(8));
        assert_eq!(parse_java_version("1.7.0_80"), Some(7));
        assert_eq!(parse_java_version("11.0.24"), Some(11));
        assert_eq!(parse_java_version("17.0.2"), Some(17));
        assert_eq!(parse_java_version("21"), Some(21));
        assert_eq!(parse_java_version("not a version"), None);
    }

    /// The whole reason this module exists: one server, many Minecraft eras.
    #[test]
    fn each_minecraft_era_gets_its_own_java() {
        for (version, want) in [
            ("1.5.2", 8),
            ("1.6.4", 8),
            ("1.7.10", 8),
            ("1.8.9", 8),
            ("1.10.2", 8),
            ("1.11.2", 8),
            ("1.12.2", 8),
            ("1.13.2", 8),
            ("1.16.5", 8),
            ("1.17", 16),
            ("1.17.1", 16),
            ("1.18.2", 17),
            ("1.19.4", 17),
            ("1.20.1", 17),
            ("1.20.4", 17),
            ("1.20.5", 21),
            ("1.20.6", 21),
            ("1.21.4", 21),
            ("1.21.5", 21),
        ] {
            assert_eq!(required_major(version, None), want, "java for {version}");
        }
    }

    #[test]
    fn snapshots_and_profiles_are_honoured() {
        assert_eq!(required_major("24w21a", None), 21);
        assert_eq!(required_major("21w03a", None), 17);
        assert_eq!(required_major("19w44a", None), 8);
        // A profile's own declaration always wins.
        assert_eq!(required_major("1.12.2", Some(8)), 8);
        assert_eq!(required_major("1.20.1", Some(21)), 21);
        assert_eq!(required_major("weird-fork", Some(17)), 17);
        // Nothing declared and nothing parseable: modern default, never Java 6.
        assert_eq!(required_major("bta-1.7", None), 17);
    }

    #[test]
    fn a_jdks_size_means_its_home_not_the_whole_filesystem() {
        let dir = tempfile::tempdir().unwrap();
        let exe = if cfg!(windows) { "java.exe" } else { "java" };
        let home = dir.path().join("java-8");
        std::fs::create_dir_all(home.join("bin")).unwrap();
        std::fs::create_dir_all(home.join("lib")).unwrap();
        std::fs::write(home.join("bin").join(exe), "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::write(home.join("release"), b"JAVA_VERSION=\"1.8.0_512\"\n").unwrap();
        std::fs::write(home.join("lib").join("modules"), vec![7u8; 4096]).unwrap();

        // A marker-bearing home is measured.
        assert!(jdk_home(&home.join("bin").join(exe)).as_deref() == Some(home.as_path()));
        assert!(jdk_size(&home.join("bin").join(exe)) >= 4096);

        // A loose script with no JDK around it reports nothing rather than
        // the size of its parent tree.
        let stray = dir.path().join(exe);
        std::fs::write(&stray, "#!/bin/sh\nexit 0\n").unwrap();
        assert_eq!(jdk_home(&stray), None);
        assert_eq!(jdk_size(&stray), 0);
    }

    #[test]
    fn host_jdks_come_from_path_then_java_home_without_duplicates() {
        let dir = tempfile::tempdir().unwrap();
        let exe = if cfg!(windows) { "java.exe" } else { "java" };
        // A normal JDK layout: JAVA_HOME=/…/home with the binary in bin/.
        let home = dir.path().join("home");
        std::fs::create_dir_all(home.join("bin")).unwrap();
        let java = home.join("bin").join(exe);
        std::fs::write(&java, "#!/bin/sh\nexit 0\n").unwrap();
        let sep = if cfg!(windows) { ';' } else { ':' };

        // Found through PATH alone.
        let found =
            system_java_candidates(&format!("/nope{sep}{}", home.join("bin").display()), None);
        assert_eq!(found, vec![java.clone()]);

        // Or through JAVA_HOME alone, even when PATH has nothing.
        assert_eq!(
            system_java_candidates("", Some(home.to_str().unwrap())),
            vec![java.clone()]
        );

        // Both routes to the same binary are collapsed into one candidate.
        let found = system_java_candidates(
            &format!("{}", home.join("bin").display()),
            Some(home.to_str().unwrap()),
        );
        assert_eq!(found, vec![java.clone()]);

        // A different JDK on PATH is listed first, JAVA_HOME second.
        let other = dir.path().join(exe);
        std::fs::write(&other, "#!/bin/sh\nexit 0\n").unwrap();
        let found = system_java_candidates(
            &format!("{}", dir.path().display()),
            Some(home.to_str().unwrap()),
        );
        assert_eq!(found, vec![other, java]);

        assert!(system_java_candidates("", Some("   ")).is_empty());
    }

    #[test]
    fn java_is_found_on_a_path_style_string() {
        let dir = tempfile::tempdir().unwrap();
        let name = if cfg!(windows) { "java.exe" } else { "java" };
        let real = dir.path().join(name);
        std::fs::write(&real, b"#!/bin/sh\nexit 0\n").unwrap();

        let sep = if cfg!(windows) { ';' } else { ':' };
        let path_var = format!("/nonexistent{sep}{}{sep}/usr/bin", dir.path().display());
        assert_eq!(java_on_path(&path_var).as_deref(), Some(real.as_path()));
        assert_eq!(java_on_path(""), None);
        assert_eq!(java_on_path("/nope:/also/nope"), None);
    }

    #[test]
    fn resolution_prefers_override_then_host_then_managed() {
        let mut registry = JreRegistry::default();
        registry.upsert(JreRecord {
            major: 8,
            path: "/opt/java-8/bin/java".into(),
            source: JreSource::Managed,
            status: JreStatus::Ready,
            version: None,
            error: None,
            updated_at: String::new(),
        });
        registry.upsert(JreRecord {
            major: 8,
            path: "/usr/lib/jvm/java-8/bin/java".into(),
            source: JreSource::System,
            status: JreStatus::Ready,
            version: None,
            error: None,
            updated_at: String::new(),
        });
        assert_eq!(registry.find(8).unwrap().source, JreSource::System);
        registry.upsert(JreRecord {
            major: 8,
            path: "/srv/pinned/bin/java".into(),
            source: JreSource::Env,
            status: JreStatus::Ready,
            version: None,
            error: None,
            updated_at: String::new(),
        });
        assert_eq!(registry.find(8).unwrap().source, JreSource::Env);
    }

    #[test]
    fn installing_slots_do_not_count_as_available() {
        let mut registry = JreRegistry::default();
        registry.upsert(JreRecord {
            major: 17,
            path: "/data/jres/java-17/bin/java".into(),
            source: JreSource::Managed,
            status: JreStatus::Installing,
            version: None,
            error: None,
            updated_at: String::new(),
        });
        assert!(registry.find(17).is_none());
        assert_eq!(registry.jres.len(), 1);
        // A finished download replaces the same managed slot, not adds one.
        registry.upsert(JreRecord {
            status: JreStatus::Ready,
            ..registry.jres[0].clone()
        });
        assert_eq!(registry.jres.len(), 1);
        assert_eq!(registry.find(17).unwrap().status, JreStatus::Ready);
    }
}
