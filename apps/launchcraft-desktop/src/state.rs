use launchcraft_auth::AccountManager;
use launchcraft_core::paths::LauncherPaths;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Global application state managed by the desktop shell.
pub struct AppState {
    pub paths: LauncherPaths,
    pub accounts: Arc<Mutex<AccountManager>>,
}

impl AppState {
    pub fn new(paths: LauncherPaths) -> anyhow::Result<Self> {
        let accounts_path = paths.config.join("accounts.json");
        let storage = AccountManager::new(accounts_path)
            .map_err(|e| anyhow::anyhow!("Failed to initialize account storage: {e}"))?;
        Ok(Self {
            paths,
            accounts: Arc::new(Mutex::new(storage)),
        })
    }
}
