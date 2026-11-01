pub mod adoptium;

use crate::error::RuntimeError;
use crate::platform::{OperatingSystem, PlatformInfo};
use std::path::{Path, PathBuf};

pub use adoptium::{
    AdoptiumBinary, AdoptiumClient, AdoptiumPackage, AdoptiumRelease, JavaDownloadProgress,
    JavaInstallPhase, JavaProgress,
};

use adoptium::find_java_executable;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum JavaMajorVersion {
    Java8 = 8,
    Java16 = 16,
    Java17 = 17,
    Java21 = 21,
}

impl JavaMajorVersion {
    pub fn as_u32(&self) -> u32 {
        *self as u32
    }
}

pub struct JavaRuntimeManager {
    runtimes_dir: PathBuf,
    platform: PlatformInfo,
    adoptium: AdoptiumClient,
}

impl JavaRuntimeManager {
    pub fn new<P: AsRef<Path>>(runtimes_dir: P) -> Self {
        Self::with_platform(runtimes_dir, PlatformInfo::current())
    }

    pub fn with_platform<P: AsRef<Path>>(runtimes_dir: P, platform: PlatformInfo) -> Self {
        Self {
            runtimes_dir: runtimes_dir.as_ref().to_path_buf(),
            platform,
            adoptium: AdoptiumClient::new(),
        }
    }

    /// Manager whose Adoptium client points at a custom API base (tests, mirrors).
    pub fn with_adoptium<P: AsRef<Path>>(
        runtimes_dir: P,
        platform: PlatformInfo,
        adoptium: AdoptiumClient,
    ) -> Self {
        Self {
            runtimes_dir: runtimes_dir.as_ref().to_path_buf(),
            platform,
            adoptium,
        }
    }

    pub fn runtimes_dir(&self) -> &Path {
        &self.runtimes_dir
    }

    pub fn get_runtime_dir(&self, version: JavaMajorVersion) -> PathBuf {
        self.runtimes_dir.join(format!("java-{}", version.as_u32()))
    }

    pub fn get_java_binary(&self, version: JavaMajorVersion) -> PathBuf {
        let dir = self.get_runtime_dir(version);
        let exec_name = if self.platform.os == OperatingSystem::Windows {
            "java.exe"
        } else {
            "java"
        };

        if self.platform.os == OperatingSystem::MacOS {
            let mac_bin = dir.join("Contents/Home/bin").join(exec_name);
            if mac_bin.exists() {
                return mac_bin;
            }
        }

        dir.join("bin").join(exec_name)
    }

    pub fn is_runtime_installed(&self, version: JavaMajorVersion) -> bool {
        self.get_java_binary(version).exists()
    }

    /// Executable of a previously provisioned managed runtime, for any major
    /// version (not just the enum's known set).
    pub fn provisioned_java(&self, major_version: u32) -> Option<PathBuf> {
        let dir = self.runtimes_dir.join(format!("java-{}", major_version));
        find_java_executable(&dir).filter(|p| p.is_file())
    }

    /// Return the managed `java` executable for `major_version`, downloading
    /// and extracting a Temurin JDK into `runtimes_dir/java-<major>` when it
    /// is not installed yet (Prism-style automatic runtime provisioning).
    pub async fn ensure_java(
        &self,
        major_version: u32,
        progress: Option<&JavaProgress>,
    ) -> Result<PathBuf, RuntimeError> {
        if let Some(existing) = self.provisioned_java(major_version) {
            if let Some(progress) = progress {
                progress(JavaDownloadProgress {
                    downloaded: 0,
                    total: Some(0),
                    phase: JavaInstallPhase::Done,
                });
            }
            return Ok(existing);
        }
        self.adoptium
            .install_into(major_version, &self.runtimes_dir, &self.platform, progress)
            .await
    }

    pub fn find_system_java(&self) -> Option<PathBuf> {
        if let Ok(java_home) = std::env::var("JAVA_HOME") {
            let bin = if self.platform.os == OperatingSystem::Windows {
                PathBuf::from(java_home).join("bin").join("java.exe")
            } else {
                PathBuf::from(java_home).join("bin").join("java")
            };
            if bin.exists() {
                return Some(bin);
            }
        }
        None
    }
}
