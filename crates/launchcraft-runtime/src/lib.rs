//! Runtime management, Mojang manifest resolution, and game process spawner.

pub mod error;
pub mod java;
pub mod mojang;
pub mod platform;
pub mod process;

pub use error::RuntimeError;
pub use java::{AdoptiumClient, JavaMajorVersion, JavaRuntimeManager};
pub use mojang::{Library, OsRule, Rule, RuleAction, VersionManifest};
pub use platform::{Architecture, OperatingSystem, PlatformInfo};
pub use process::{GameProcess, LaunchConfig, ProcessSpawner};
