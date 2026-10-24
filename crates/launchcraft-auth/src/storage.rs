use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use uuid::Uuid;

use crate::error::{AuthError, Result};
use crate::models::LaunchcraftAccount;

/// Account storage container saved to disk
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct AccountStoreFile {
    pub active_account: Option<Uuid>,
    pub accounts: HashMap<Uuid, LaunchcraftAccount>,
}

/// Token & Account store manager supporting portable file cache and secure persistence
pub struct AccountManager {
    storage_path: PathBuf,
    store: RwLock<AccountStoreFile>,
}

impl AccountManager {
    pub fn new(storage_path: PathBuf) -> Result<Self> {
        let store = if storage_path.exists() {
            let data = fs::read_to_string(&storage_path)?;
            serde_json::from_str(&data).unwrap_or_default()
        } else {
            AccountStoreFile::default()
        };

        Ok(Self {
            storage_path,
            store: RwLock::new(store),
        })
    }

    /// Save state to disk
    fn persist(&self) -> Result<()> {
        if let Some(parent) = self.storage_path.parent() {
            fs::create_dir_all(parent)?;
        }

        let guard = self.store.read().map_err(|e| AuthError::Storage(e.to_string()))?;
        let json = serde_json::to_string_pretty(&*guard)?;
        fs::write(&self.storage_path, json)?;
        Ok(())
    }

    /// Add or update an account in store
    pub fn save_account(&self, account: LaunchcraftAccount) -> Result<()> {
        let uuid = account.uuid;
        {
            let mut guard = self.store.write().map_err(|e| AuthError::Storage(e.to_string()))?;
            if guard.active_account.is_none() {
                guard.active_account = Some(uuid);
            }
            guard.accounts.insert(uuid, account);
        }
        self.persist()
    }

    /// Retrieve account by UUID
    pub fn get_account(&self, uuid: &Uuid) -> Result<LaunchcraftAccount> {
        let guard = self.store.read().map_err(|e| AuthError::Storage(e.to_string()))?;
        guard
            .accounts
            .get(uuid)
            .cloned()
            .ok_or_else(|| AuthError::AccountNotFound(uuid.to_string()))
    }

    /// List all stored accounts
    pub fn list_accounts(&self) -> Result<Vec<LaunchcraftAccount>> {
        let guard = self.store.read().map_err(|e| AuthError::Storage(e.to_string()))?;
        Ok(guard.accounts.values().cloned().collect())
    }

    /// Get active account
    pub fn get_active_account(&self) -> Result<Option<LaunchcraftAccount>> {
        let guard = self.store.read().map_err(|e| AuthError::Storage(e.to_string()))?;
        if let Some(uuid) = guard.active_account {
            Ok(guard.accounts.get(&uuid).cloned())
        } else {
            Ok(None)
        }
    }

    /// Set active account
    pub fn set_active_account(&self, uuid: Uuid) -> Result<()> {
        {
            let mut guard = self.store.write().map_err(|e| AuthError::Storage(e.to_string()))?;
            if !guard.accounts.contains_key(&uuid) {
                return Err(AuthError::AccountNotFound(uuid.to_string()));
            }
            guard.active_account = Some(uuid);
        }
        self.persist()
    }

    /// Remove an account
    pub fn remove_account(&self, uuid: &Uuid) -> Result<()> {
        {
            let mut guard = self.store.write().map_err(|e| AuthError::Storage(e.to_string()))?;
            guard.accounts.remove(uuid);
            if guard.active_account == Some(*uuid) {
                guard.active_account = guard.accounts.keys().next().copied();
            }
        }
        self.persist()
    }
}
