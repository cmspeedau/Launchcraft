//! Desktop application entry point for Launchcraft.

use launchcraft_core::paths::LauncherPaths;
use launchcraft_desktop::state::AppState;
use launchcraft_desktop::commands;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let exe_dir = std::env::current_exe()
        .map(|p| p.parent().unwrap_or(&p).to_path_buf())
        .unwrap_or_else(|_| std::path::PathBuf::from("."));

    let paths = LauncherPaths::discover(&exe_dir);
    paths.ensure_directories().expect("Failed to initialize launcher filesystem directories");

    let state = AppState::new(paths);
    let info = commands::get_system_info(&state).await.unwrap();
    tracing::info!("Launchcraft initialized (portable: {}, root: {:?})", info.is_portable, info.root_dir);
}
