//! Runtime management, Mojang manifest resolution, and game process spawner.

pub mod error;
pub mod java;
pub mod meta;
pub mod mojang;
pub mod platform;
pub mod process;

pub use error::RuntimeError;
pub use java::{
    AdoptiumClient, JavaDownloadProgress, JavaInstallPhase, JavaMajorVersion, JavaProgress,
    JavaRuntimeManager,
};
pub use meta::{
    InstallProgress, InstallReport, LaunchIdentity, LaunchPlan, LaunchSpec, LoaderKind,
    MinecraftClient, Progress, VersionListEntry, VersionListFile,
};
pub use mojang::{
    Arguments, AssetIndex, AssetIndexRef, AssetObject, DownloadInfo, GameArg, GameArgs,
    JavaVersionInfo, Library, LibraryDownloads, MergedProfile, OsRule, Rule, RuleAction,
    VersionManifest,
};
pub use platform::{Architecture, OperatingSystem, PlatformInfo};
pub use process::{GameProcess, LaunchConfig, ProcessSpawner};
