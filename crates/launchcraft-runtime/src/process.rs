use std::path::PathBuf;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use crate::error::RuntimeError;
use crate::platform::{OperatingSystem, PlatformInfo};

#[derive(Debug, Clone)]
pub struct LaunchConfig {
    pub java_binary: PathBuf,
    pub min_memory_mb: u32,
    pub max_memory_mb: u32,
    pub jvm_args: Vec<String>,
    pub game_dir: PathBuf,
    pub assets_dir: PathBuf,
    pub asset_index: String,
    pub version_id: String,
    pub main_class: String,
    pub classpath: Vec<PathBuf>,
    pub natives_dir: Option<PathBuf>,
    pub auth_player_name: String,
    pub auth_uuid: String,
    pub auth_token: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

impl LaunchConfig {
    pub fn build_args(&self, platform: &PlatformInfo) -> Vec<String> {
        let mut args = Vec::new();

        // Memory configuration
        args.push(format!("-Xms{}m", self.min_memory_mb));
        args.push(format!("-Xmx{}m", self.max_memory_mb));

        // macOS thread hook for GLFW
        if platform.os == OperatingSystem::MacOS {
            args.push("-XstartOnFirstThread".to_string());
            args.push(format!("-Xdock:name={}", self.version_id));
        }

        // Natives library path
        if let Some(ref natives) = self.natives_dir {
            args.push(format!("-Djava.library.path={}", natives.display()));
        }

        // Custom JVM arguments
        for arg in &self.jvm_args {
            args.push(arg.clone());
        }

        // Classpath construction
        let sep = if platform.os == OperatingSystem::Windows { ";" } else { ":" };
        let cp_joined = self
            .classpath
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(sep);

        args.push("-cp".to_string());
        args.push(cp_joined);

        // Main game class
        args.push(self.main_class.clone());

        // Minecraft game launch arguments
        args.push("--version".to_string());
        args.push(self.version_id.clone());
        args.push("--gameDir".to_string());
        args.push(self.game_dir.to_string_lossy().into_owned());
        args.push("--assetsDir".to_string());
        args.push(self.assets_dir.to_string_lossy().into_owned());
        args.push("--assetIndex".to_string());
        args.push(self.asset_index.clone());
        args.push("--username".to_string());
        args.push(self.auth_player_name.clone());
        args.push("--uuid".to_string());
        args.push(self.auth_uuid.clone());
        args.push("--accessToken".to_string());
        args.push(self.auth_token.clone());
        args.push("--userType".to_string());
        args.push("mojang".to_string());

        if let Some(w) = self.width {
            args.push("--width".to_string());
            args.push(w.to_string());
        }
        if let Some(h) = self.height {
            args.push("--height".to_string());
            args.push(h.to_string());
        }

        args
    }
}

pub struct GameProcess {
    child: Child,
}

impl GameProcess {
    pub fn id(&self) -> Option<u32> {
        self.child.id()
    }

    pub async fn wait(&mut self) -> Result<std::process::ExitStatus, RuntimeError> {
        self.child.wait().await.map_err(RuntimeError::Io)
    }
}

pub struct ProcessSpawner;

impl ProcessSpawner {
    pub fn spawn<F>(config: LaunchConfig, platform: PlatformInfo, mut log_callback: F) -> Result<GameProcess, RuntimeError>
    where
        F: FnMut(String) + Send + 'static,
    {
        let args = config.build_args(&platform);

        let mut cmd = Command::new(&config.java_binary);
        cmd.args(&args)
            .current_dir(&config.game_dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd.spawn().map_err(RuntimeError::Io)?;

        if let Some(stdout) = child.stdout.take() {
            let mut reader = BufReader::new(stdout).lines();
            tokio::spawn(async move {
                while let Ok(Some(line)) = reader.next_line().await {
                    log_callback(line);
                }
            });
        }

        Ok(GameProcess { child })
    }
}
