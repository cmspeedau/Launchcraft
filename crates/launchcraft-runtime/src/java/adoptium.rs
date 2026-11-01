use crate::error::RuntimeError;
use crate::platform::PlatformInfo;
use futures_util::StreamExt;
use reqwest::Client;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::io::{BufReader, Read};
use std::path::{Component, Path, PathBuf};

/// Largest Java package we will download (Adoptium JDK archives are ~200 MB).
const MAX_JAVA_PACKAGE_BYTES: u64 = 1_073_741_824;
/// Largest expanded JDK we will extract.
const MAX_EXTRACTED_BYTES: u64 = 3_221_225_472;
/// Largest number of archive entries we accept.
const MAX_ARCHIVE_ENTRIES: u64 = 50_000;

#[derive(Debug, Deserialize)]
pub struct AdoptiumBinary {
    pub package: Option<AdoptiumPackage>,
}

#[derive(Debug, Deserialize)]
pub struct AdoptiumPackage {
    pub link: String,
    pub checksum: Option<String>,
    pub size: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub struct AdoptiumRelease {
    pub release_name: String,
    /// The `/assets/latest` endpoint returns one item per binary with a
    /// singular `binary` field; older/other shapes carry a `binaries` array.
    #[serde(default)]
    pub binary: Option<AdoptiumBinary>,
    #[serde(default)]
    pub binaries: Vec<AdoptiumBinary>,
}

impl AdoptiumRelease {
    fn all_binaries(&self) -> impl Iterator<Item = &AdoptiumBinary> {
        self.binary.iter().chain(self.binaries.iter())
    }
}

/// Progress phases reported while provisioning a managed Java runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JavaInstallPhase {
    Checking,
    Downloading,
    Extracting,
    Done,
}

#[derive(Debug, Clone)]
pub struct JavaDownloadProgress {
    pub downloaded: u64,
    pub total: Option<u64>,
    pub phase: JavaInstallPhase,
}

/// Thread-safe progress callback for [`AdoptiumClient::install_into`].
pub type JavaProgress = std::sync::Arc<dyn Fn(JavaDownloadProgress) + Send + Sync>;

fn report(
    progress: Option<&JavaProgress>,
    downloaded: u64,
    total: Option<u64>,
    phase: JavaInstallPhase,
) {
    if let Some(cb) = progress {
        cb(JavaDownloadProgress {
            downloaded,
            total,
            phase,
        });
    }
}

pub struct AdoptiumClient {
    client: Client,
    base_url: String,
}

impl Default for AdoptiumClient {
    fn default() -> Self {
        Self::new()
    }
}

impl AdoptiumClient {
    pub fn new() -> Self {
        Self::with_base_url(
            std::env::var("LAUNCHCRAFT_ADOPTIUM_URL")
                .unwrap_or_else(|_| "https://api.adoptium.net/v3".to_string()),
        )
    }

    pub fn with_base_url<U: Into<String>>(base_url: U) -> Self {
        Self {
            client: Client::builder().build().unwrap_or_default(),
            base_url: base_url.into(),
        }
    }

    pub fn build_download_url(&self, major_version: u32, platform: &PlatformInfo) -> String {
        format!(
            "{}/binary/latest/{}/hotspot?os={}&architecture={}&image_type=jdk",
            self.base_url.trim_end_matches('/'),
            major_version,
            platform.adoptium_os_name(),
            platform.adoptium_arch_name()
        )
    }

    /// Assets API for the newest GA build of a major version on this platform.
    pub fn build_assets_url(&self, major_version: u32, platform: &PlatformInfo) -> String {
        format!(
            "{}/assets/latest/{}/hotspot?os={}&architecture={}&image_type=jdk",
            self.base_url.trim_end_matches('/'),
            major_version,
            platform.adoptium_os_name(),
            platform.adoptium_arch_name()
        )
    }

    /// Download and extract the newest Temurin JDK for `major_version` into
    /// `runtimes_dir/java-<major>` (Prism-style managed runtimes) and return
    /// the path to its `java` executable. The package is sha256-verified and
    /// archive paths are checked for traversal before anything is written.
    pub async fn install_into(
        &self,
        major_version: u32,
        runtimes_dir: &Path,
        platform: &PlatformInfo,
        progress: Option<&JavaProgress>,
    ) -> Result<PathBuf, RuntimeError> {
        report(progress, 0, None, JavaInstallPhase::Checking);
        let url = self.build_assets_url(major_version, platform);
        let resp =
            self.client.get(&url).send().await.map_err(|e| {
                RuntimeError::JavaInstall(format!("adoptium assets {} : {}", url, e))
            })?;
        let status = resp.status();
        if !status.is_success() {
            return Err(RuntimeError::JavaInstall(format!(
                "adoptium assets {} -> HTTP {}",
                url, status
            )));
        }
        let releases: Vec<AdoptiumRelease> = resp
            .json()
            .await
            .map_err(|e| RuntimeError::JavaInstall(format!("adoptium assets json: {}", e)))?;

        let pkg = releases
            .iter()
            .flat_map(|r| r.all_binaries())
            .filter_map(|b| b.package.as_ref())
            .find(|p| p.link.ends_with(".tar.gz") || p.link.ends_with(".zip"))
            .ok_or_else(|| {
                RuntimeError::JavaNotFound(format!(
                    "no downloadable JDK for Java {} on this platform",
                    major_version
                ))
            })?;

        let total = pkg.size;
        let package_url = pkg.link.clone();
        let expected_sha = pkg
            .checksum
            .as_deref()
            .map(|c| c.trim().to_ascii_lowercase());
        tracing::info!("Provisioning Java {} from {}", major_version, package_url);

        std::fs::create_dir_all(runtimes_dir)?;

        // ---- download (streamed, size-capped) ----
        report(progress, 0, total, JavaInstallPhase::Downloading);
        let id = next_temp_id();
        let tmp_pkg = runtimes_dir.join(format!(".pkg-{}.tmp", id));
        if let Err(e) = self
            .download_to_file(&package_url, &tmp_pkg, total, progress)
            .await
        {
            let _ = std::fs::remove_file(&tmp_pkg);
            return Err(e);
        }

        // ---- sha256 verify ----
        if let Some(expected) = &expected_sha {
            let actual = sha256_hex(&tmp_pkg)?;
            if &actual != expected {
                let _ = std::fs::remove_file(&tmp_pkg);
                return Err(RuntimeError::JavaInstall(format!(
                    "Java package sha256 mismatch: expected {}, got {}",
                    expected, actual
                )));
            }
        }

        // ---- extract into a staging dir, then move into place ----
        let downloaded = std::fs::metadata(&tmp_pkg).map(|m| m.len()).unwrap_or(0);
        report(
            progress,
            downloaded,
            Some(downloaded),
            JavaInstallPhase::Extracting,
        );
        let target = runtimes_dir.join(format!("java-{}", major_version));
        let stage = runtimes_dir.join(format!(".stage-{}.tmp", id));
        let result = (|| -> Result<(), RuntimeError> {
            std::fs::create_dir_all(&stage)?;
            if package_url.ends_with(".zip") {
                extract_zip(&tmp_pkg, &stage)?;
            } else {
                extract_tar_gz(&tmp_pkg, &stage)?;
            }
            strip_top_level(&stage)?;
            if target.exists() {
                let _ = std::fs::remove_dir_all(&target);
            }
            std::fs::rename(&stage, &target)?;
            Ok(())
        })();
        let _ = std::fs::remove_file(&tmp_pkg);
        let _ = std::fs::remove_dir_all(&stage);
        result?;

        let exe = find_java_executable(&target).ok_or_else(|| {
            RuntimeError::JavaInstall(format!(
                "extracted Java {} has no bin/java executable",
                major_version
            ))
        })?;
        report(
            progress,
            downloaded,
            Some(downloaded),
            JavaInstallPhase::Done,
        );
        tracing::info!(
            "Installed managed Java {} at {}",
            major_version,
            exe.display()
        );
        Ok(exe)
    }

    async fn download_to_file(
        &self,
        url: &str,
        dest: &Path,
        total: Option<u64>,
        progress: Option<&JavaProgress>,
    ) -> Result<(), RuntimeError> {
        use tokio::io::AsyncWriteExt;
        let mut file = tokio::fs::File::create(dest).await?;
        let resp = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|e| RuntimeError::JavaInstall(format!("{} : {}", url, e)))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(RuntimeError::JavaInstall(format!(
                "{} -> HTTP {}",
                url, status
            )));
        }
        let mut downloaded: u64 = 0;
        let mut last_reported: u64 = 0;
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| RuntimeError::JavaInstall(e.to_string()))?;
            downloaded += chunk.len() as u64;
            if downloaded > MAX_JAVA_PACKAGE_BYTES {
                return Err(RuntimeError::JavaInstall(format!(
                    "Java package exceeds {} bytes",
                    MAX_JAVA_PACKAGE_BYTES
                )));
            }
            file.write_all(&chunk).await?;
            if downloaded.saturating_sub(last_reported) >= 65_536 {
                last_reported = downloaded;
                report(progress, downloaded, total, JavaInstallPhase::Downloading);
            }
        }
        file.flush().await?;
        Ok(())
    }
}

fn sha256_hex(path: &Path) -> Result<String, RuntimeError> {
    let data = std::fs::read(path)?;
    let mut hasher = Sha256::new();
    hasher.update(&data);
    let digest = hasher.finalize();
    Ok(digest.iter().map(|b| format!("{:02x}", b)).collect())
}

fn next_temp_id() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

/// Reject archive paths that could escape the extraction directory.
fn validate_rel_path(path: &Path) -> Result<(), RuntimeError> {
    if path.as_os_str().is_empty() || path.is_absolute() {
        return Err(RuntimeError::JavaInstall(format!(
            "archive entry has absolute or empty path: {}",
            path.display()
        )));
    }
    for comp in path.components() {
        match comp {
            Component::Normal(_) | Component::CurDir => {}
            _ => {
                return Err(RuntimeError::JavaInstall(format!(
                    "archive entry has unsafe path: {}",
                    path.display()
                )))
            }
        }
    }
    Ok(())
}

/// True if any path component is a directory named `bin`.
fn under_bin_dir(path: &Path) -> bool {
    path.components().any(|c| match c {
        Component::Normal(s) => s == "bin",
        _ => false,
    })
}

/// Zip archives do not carry unix modes; make everything under a `bin/`
/// directory executable so `bin/java` runs after extraction.
#[cfg(unix)]
fn mark_executables(dir: &Path) {
    fn walk(dir: &Path) {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path);
                } else if under_bin_dir(&path) {
                    use std::os::unix::fs::PermissionsExt;
                    if let Ok(meta) = std::fs::metadata(&path) {
                        let mode = meta.permissions().mode() | 0o111;
                        let _ =
                            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode));
                    }
                }
            }
        }
    }
    walk(dir);
}

#[cfg(not(unix))]
fn mark_executables(_dir: &Path) {}

fn extract_zip(archive: &Path, dest: &Path) -> Result<(), RuntimeError> {
    let file = std::fs::File::open(archive)?;
    let mut zip = zip::ZipArchive::new(BufReader::new(file))?;
    if zip.len() as u64 > MAX_ARCHIVE_ENTRIES {
        return Err(RuntimeError::JavaInstall("zip has too many entries".into()));
    }
    let mut written: u64 = 0;
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i)?;
        let rel = match entry.enclosed_name() {
            Some(p) => p.to_path_buf(),
            None => continue,
        };
        validate_rel_path(&rel)?;
        let path = dest.join(&rel);
        if entry.is_dir() {
            std::fs::create_dir_all(&path)?;
            continue;
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut out = std::fs::File::create(&path)?;
        let mut buf = [0u8; 65_536];
        loop {
            let n = entry.read(&mut buf)?;
            if n == 0 {
                break;
            }
            written += n as u64;
            if written > MAX_EXTRACTED_BYTES {
                return Err(RuntimeError::JavaInstall(
                    "zip expands beyond size limit".into(),
                ));
            }
            std::io::Write::write_all(&mut out, &buf[..n])?;
        }
    }
    mark_executables(dest);
    Ok(())
}

fn extract_tar_gz(archive: &Path, dest: &Path) -> Result<(), RuntimeError> {
    let file = std::fs::File::open(archive)?;
    let gz = flate2::read::GzDecoder::new(BufReader::new(file));
    let mut ar = tar::Archive::new(gz);
    let mut entries: u64 = 0;
    let mut written: u64 = 0;
    for entry in ar.entries()? {
        let mut entry = entry?;
        entries += 1;
        if entries > MAX_ARCHIVE_ENTRIES {
            return Err(RuntimeError::JavaInstall("tar has too many entries".into()));
        }
        let rel = entry.path()?.into_owned();
        validate_rel_path(&rel)?;
        let kind = entry.header().entry_type();
        let path = dest.join(&rel);
        match kind {
            tar::EntryType::Directory => {
                std::fs::create_dir_all(&path)?;
            }
            tar::EntryType::Regular => {
                written += entry.header().entry_size().unwrap_or(0);
                if written > MAX_EXTRACTED_BYTES {
                    return Err(RuntimeError::JavaInstall(
                        "tar expands beyond size limit".into(),
                    ));
                }
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                // tar preserves unix modes, so bin/java keeps its exec bit.
                entry.unpack(&path)?;
            }
            _ => {
                // Symlinks/hardlinks/devices: skip (running the JDK needs none).
                tracing::debug!("skipping tar entry {} ({:?})", rel.display(), kind);
            }
        }
    }
    mark_executables(dest);
    Ok(())
}

/// If extraction produced a single top-level directory (JDK archives nest
/// everything under `jdk-21.0.5+11/`), move its contents up one level.
fn strip_top_level(dir: &Path) -> Result<(), RuntimeError> {
    let mut entries: Vec<PathBuf> = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        entries.push(entry?.path());
    }
    if entries.len() != 1 || !entries[0].is_dir() {
        return Ok(());
    }
    let inner = entries[0].clone();
    let renamed = dir.with_extension("stripped");
    std::fs::rename(&inner, &renamed)?;
    std::fs::remove_dir(dir)?;
    std::fs::rename(&renamed, dir)?;
    Ok(())
}

/// Locate the `java` executable inside an extracted JDK directory, tolerating
/// the macOS `Contents/Home` layout.
pub(crate) fn find_java_executable(dir: &Path) -> Option<PathBuf> {
    let name = if cfg!(windows) { "java.exe" } else { "java" };
    for layout in ["bin", "Contents/Home/bin"] {
        let p = dir.join(layout).join(name);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}
