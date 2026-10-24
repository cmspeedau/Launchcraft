pub mod adoptium;

use std::path::{Path, PathBuf};
use crate::error::RuntimeError;
use crate::platform::{OperatingSystem, PlatformInfo};

pub use adoptium::AdoptiumClient;

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
        Self {
            runtimes_dir: runtimes_dir.as_ref().to_path_buf(),
            platform: PlatformInfo::current(),
            adoptium: AdoptiumClient::new(),
        }
    }

    pub fn with_platform<P: AsRef<Path>>(runtimes_dir: P, platform: PlatformInfo) -> Self {
        Self {
            runtimes_dir: runtimes_dir.as_ref().to_path_buf(),
            platform,
            adoptium: AdoptiumClient::new(),
        }
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
