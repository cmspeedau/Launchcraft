use directories::ProjectDirs;
use std::path::PathBuf;

/// Handles resolution of directories across Linux, Windows, and macOS,
/// with automatic detection of portable mode.
#[derive(Debug, Clone)]
pub struct LauncherPaths {
    pub root: PathBuf,
    pub instances: PathBuf,
    pub cas_cache: PathBuf,
    pub runtimes: PathBuf,
    /// Mojang-style `versions/<id>/` store for downloaded game versions.
    pub versions: PathBuf,
    /// Mojang-style `assets/` store (indexes + objects).
    pub assets: PathBuf,
    /// Shared Maven-layout `libraries/` store.
    pub libraries: PathBuf,
    pub config: PathBuf,
    pub is_portable: bool,
}

impl LauncherPaths {
    /// Resolves the runtime paths. If a `data` folder or `.portable` marker
    /// is present next to the executable, Launchcraft operates in portable mode.
    pub fn resolve() -> anyhow::Result<Self> {
        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|p| p.to_path_buf()))
            .unwrap_or_else(|| PathBuf::from("."));

        let portable_dir = exe_dir.join("data");
        let portable_marker = exe_dir.join(".portable");

        let (root, config_dir, is_portable) = if portable_dir.exists() || portable_marker.exists() {
            (portable_dir.clone(), portable_dir.join("config"), true)
        } else {
            let proj = ProjectDirs::from("com", "Launchcraft", "Launchcraft")
                .ok_or_else(|| anyhow::anyhow!("Failed to resolve standard OS directories"))?;
            (
                proj.data_dir().to_path_buf(),
                proj.config_dir().to_path_buf(),
                false,
            )
        };

        Ok(Self {
            instances: root.join("instances"),
            cas_cache: root.join("cas"),
            runtimes: root.join("runtimes"),
            versions: root.join("versions"),
            assets: root.join("assets"),
            libraries: root.join("libraries"),
            config: config_dir,
            root,
            is_portable,
        })
    }

    /// Ensures that all base directories exist.
    pub fn ensure_dirs(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.root)?;
        std::fs::create_dir_all(&self.instances)?;
        std::fs::create_dir_all(&self.cas_cache)?;
        std::fs::create_dir_all(&self.runtimes)?;
        std::fs::create_dir_all(&self.versions)?;
        std::fs::create_dir_all(&self.assets)?;
        std::fs::create_dir_all(&self.libraries)?;
        std::fs::create_dir_all(&self.config)?;
        Ok(())
    }
}
