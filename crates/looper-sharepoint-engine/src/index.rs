//! `<data_dir>/state/index.json`: maps each configured library's `local` name to its drive and state DB, so offline
//! commands like `status` can find the DB without calling Graph, and runs don't re-resolve the site and library each
//! time (that cost 3 RU per run on top of a 1 RU delta; see `docs/sharepoint/throttling.md`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use looper_sharepoint_core::config::Library;
use serde::{Deserialize, Serialize};

use crate::sync::Resolved;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub drive_id: String,
    pub db_path: PathBuf,
    /// What `drive_id` was resolved from (config `site` / `library`) and the site id. Older entries lack these and
    /// are re-resolved once.
    #[serde(default)]
    pub site: Option<String>,
    #[serde(default)]
    pub library: Option<String>,
    #[serde(default)]
    pub site_id: Option<String>,
    /// The config's `folder`, and the folder's item id it was resolved to.
    #[serde(default)]
    pub folder: Option<String>,
    #[serde(default)]
    pub folder_id: Option<String>,
}

fn file(data_dir: &Path) -> PathBuf {
    data_dir.join("state").join("index.json")
}

pub fn load(data_dir: &Path) -> Result<BTreeMap<String, Entry>> {
    match std::fs::read_to_string(file(data_dir)) {
        Ok(s) => Ok(serde_json::from_str(&s)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(e) => Err(e.into()),
    }
}

pub fn record(data_dir: &Path, lib: &Library, resolved: &Resolved, db_path: &Path) -> Result<()> {
    let mut idx = load(data_dir)?;
    idx.insert(
        lib.local.to_lowercase(),
        Entry {
            drive_id: resolved.drive_id.clone(),
            db_path: db_path.into(),
            site: Some(lib.site.clone()),
            library: Some(lib.library.clone()),
            site_id: Some(resolved.site_id.clone()),
            folder: lib.folder.clone(),
            folder_id: resolved.folder_id.clone(),
        },
    );
    save(data_dir, &idx)
}

/// The site and drive ids resolved earlier for exactly this config entry, if any.
pub fn cached(data_dir: &Path, lib: &Library) -> Option<Resolved> {
    let e = load(data_dir).ok()?.remove(&lib.local.to_lowercase())?;
    (e.site.as_deref() == Some(lib.site.as_str())
        && e.library.as_deref() == Some(lib.library.as_str())
        && e.folder == lib.folder
        && e.folder.is_some() == e.folder_id.is_some())
        .then_some(Resolved { site_id: e.site_id?, drive_id: e.drive_id, folder_id: e.folder_id })
}

/// Forget the cached resolution (the drive went away); the next run resolves again. Keeps the DB path for `status`.
pub fn invalidate(data_dir: &Path, local: &str) -> Result<()> {
    let mut idx = load(data_dir)?;
    if let Some(e) = idx.get_mut(&local.to_lowercase()) {
        e.site = None;
        e.library = None;
        save(data_dir, &idx)?;
    }
    Ok(())
}

fn save(data_dir: &Path, idx: &BTreeMap<String, Entry>) -> Result<()> {
    std::fs::create_dir_all(data_dir.join("state"))?;
    std::fs::write(file(data_dir), serde_json::to_string_pretty(idx)?)?;
    Ok(())
}
