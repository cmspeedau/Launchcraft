//! Desktop application entry point for Launchcraft.

use launchcraft_core::paths::LauncherPaths;
use launchcraft_desktop::commands;
use launchcraft_desktop::state::AppState;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let paths = LauncherPaths::resolve().expect("Failed to resolve launcher directories");
    paths
        .ensure_dirs()
        .expect("Failed to initialize launcher filesystem directories");

    let state = AppState::new(paths).expect("Failed to initialize application state");
    let info = commands::get_system_info(&state).await.unwrap();
    tracing::info!(
        "Launchcraft initialized (portable: {}, root: {:?})",
        info.is_portable,
        info.root_dir
    );
}
