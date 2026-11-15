//! Pack registry: the server-side equivalent of SKCraft's Modpack Creator
//! workspace. Each entry names a pack, its optional source tree (what the
//! builder zips) and its built `dist` directory (what `repo-server` serves).
//!
//! The registry is a plain JSON file (`<data_dir>/packs.json`). Removing a
//! pack unregisters it; files on disk are never touched.

use std::fs;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::store::write_atomic;

#[derive(Debug, Error)]
pub enum RegistryError {
    #[error("pack id already registered: {0}")]
    AlreadyRegistered(String),
    #[error("pack not registered: {0}")]
    Unknown(String),
    #[error("invalid pack id: {0} (use letters, digits, '_', '-', '.', max 64 chars)")]
    InvalidId(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

/// Where a pack was imported from, so the console can re-import a newer
/// version in place. Ids are stored as strings because every provider
/// counts differently: CurseForge numeric project/file ids, Modrinth slug +
/// version id, FTB pack dir + version token.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackSource {
    /// "curseforge" | "modrinth" | "ftb"
    pub provider: String,
    /// Provider-side project handle (project id / slug / pack dir).
    pub project: String,
    /// The provider file/version last imported; an update bumps this.
    pub file: String,
}

/// One row of the Pack Manager table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackEntry {
    /// Builder pack id; also the registry key.
    pub id: String,
    /// Operator-facing label (SKCraft's "Title" column).
    pub title: String,
    /// Source tree fed to `launchcraft-builder` (None for prebuilt dists).
    #[serde(default)]
    pub src: Option<PathBuf>,
    /// Built distribution directory holding `manifest.json` + `cas/objects/`.
    pub dist: PathBuf,
    #[serde(default)]
    pub created_at: String,
    /// Set for packs imported from a modpack host; drives the update button.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<PackSource>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct PackRegistry {
    #[serde(default)]
    pub packs: Vec<PackEntry>,
}

/// Validates a pack id: a plain, filesystem-safe token.
pub fn validate_pack_id(id: &str) -> Result<(), RegistryError> {
    let ok = !id.is_empty()
        && id.len() <= 64
        && !id.starts_with('.')
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'));
    if ok {
        Ok(())
    } else {
        Err(RegistryError::InvalidId(id.to_string()))
    }
}

impl PackRegistry {
    pub fn path(data_dir: &std::path::Path) -> PathBuf {
        data_dir.join("packs.json")
    }

    pub fn load(data_dir: &std::path::Path) -> Result<Self, RegistryError> {
        let path = Self::path(data_dir);
        if !path.exists() {
            return Ok(Self::default());
        }
        Ok(serde_json::from_slice(&fs::read(path)?)?)
    }

    pub fn save(&self, data_dir: &std::path::Path) -> Result<(), RegistryError> {
        fs::create_dir_all(data_dir)?;
        let body = serde_json::to_vec_pretty(self)?;
        write_atomic(&Self::path(data_dir), &body)?;
        Ok(())
    }

    pub fn find(&self, id: &str) -> Option<&PackEntry> {
        self.packs.iter().find(|p| p.id == id)
    }

    pub fn add(&mut self, entry: PackEntry) -> Result<(), RegistryError> {
        validate_pack_id(&entry.id)?;
        if self.find(&entry.id).is_some() {
            return Err(RegistryError::AlreadyRegistered(entry.id));
        }
        self.packs.push(entry);
        self.packs.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(())
    }

    pub fn remove(&mut self, id: &str) -> Result<PackEntry, RegistryError> {
        let idx = self
            .packs
            .iter()
            .position(|p| p.id == id)
            .ok_or_else(|| RegistryError::Unknown(id.to_string()))?;
        Ok(self.packs.remove(idx))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str) -> PackEntry {
        PackEntry {
            id: id.into(),
            title: format!("{id} title"),
            src: None,
            dist: PathBuf::from("/srv/dist").join(id),
            created_at: String::new(),
            source: None,
        }
    }

    #[test]
    fn id_validation() {
        assert!(validate_pack_id("demo-skyblock").is_ok());
        assert!(validate_pack_id("alice_1.7.v2").is_ok());
        for bad in [
            "",
            "..",
            "../etc",
            "with space",
            "sl/ash",
            "x".repeat(65).as_str(),
        ] {
            assert!(validate_pack_id(bad).is_err(), "must reject {bad:?}");
        }
    }

    #[test]
    fn registry_roundtrip_and_mutations() {
        let tmp = tempfile::tempdir().unwrap();
        let mut reg = PackRegistry::load(tmp.path()).unwrap();
        assert!(reg.packs.is_empty());

        reg.add(entry("alpha")).unwrap();
        reg.add(entry("beta")).unwrap();
        assert!(reg.add(entry("alpha")).is_err());
        reg.save(tmp.path()).unwrap();

        let reloaded = PackRegistry::load(tmp.path()).unwrap();
        assert_eq!(reloaded.packs.len(), 2);
        assert_eq!(reloaded.packs[0].id, "alpha");
        assert_eq!(reloaded.packs[0].title, "alpha title");

        let mut reg = reloaded;
        assert_eq!(reg.remove("alpha").unwrap().id, "alpha");
        assert!(reg.remove("alpha").is_err());
        reg.save(tmp.path()).unwrap();
        assert_eq!(PackRegistry::load(tmp.path()).unwrap().packs.len(), 1);
    }
}
