use std::sync::Arc;
use tokio::sync::Mutex;
use launchcraft_core::paths::LauncherPaths;
use launchcraft_auth::storage::AccountStorage;

/// Global application state managed by the desktop shell.
pub struct AppState {
    pub paths: LauncherPaths,
    pub accounts: Arc<Mutex<AccountStorage>>,
}

impl AppState {
    pub fn new(paths: LauncherPaths) -> Self {
        let accounts_path = paths.accounts_file();
        let storage = AccountStorage::new(accounts_path);
        Self {
            paths,
            accounts: Arc::new(Mutex::new(storage)),
        }
    }
}
