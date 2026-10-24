use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OperatingSystem {
    Windows,
    MacOS,
    Linux,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Architecture {
    X86_64,
    AArch64,
    Arm,
    X86,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlatformInfo {
    pub os: OperatingSystem,
    pub arch: Architecture,
}

impl PlatformInfo {
    pub fn current() -> Self {
        let os = match std::env::consts::OS {
            "windows" => OperatingSystem::Windows,
            "macos" => OperatingSystem::MacOS,
            "linux" => OperatingSystem::Linux,
            _ => OperatingSystem::Unknown,
        };

        let arch = match std::env::consts::ARCH {
            "x86_64" => Architecture::X86_64,
            "aarch64" => Architecture::AArch64,
            "arm" => Architecture::Arm,
            "x86" => Architecture::X86,
            _ => Architecture::Unknown,
        };

        Self { os, arch }
    }

    pub fn adoptium_os_name(&self) -> &'static str {
        match self.os {
            OperatingSystem::Windows => "windows",
            OperatingSystem::MacOS => "mac",
            OperatingSystem::Linux => "linux",
            OperatingSystem::Unknown => "unknown",
        }
    }

    pub fn adoptium_arch_name(&self) -> &'static str {
        match self.arch {
            Architecture::X86_64 => "x64",
            Architecture::AArch64 => "aarch64",
            Architecture::Arm => "arm",
            Architecture::X86 => "x32",
            Architecture::Unknown => "unknown",
        }
    }

    pub fn mojang_os_name(&self) -> &'static str {
        match self.os {
            OperatingSystem::Windows => "windows",
            OperatingSystem::MacOS => "osx",
            OperatingSystem::Linux => "linux",
            OperatingSystem::Unknown => "unknown",
        }
    }
}
