//! Setting up and adding libraries for the user: the part of `setup` and `add` that both the CLI and the tray use.
//! Plans are worked out first (so they can be shown and confirmed), then applied. Every change to the config file is
//! checked as a whole before it's written; see `looper_sharepoint_core::config_edit`. `specs/onboarding-design.md`
//! §4–§5.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use looper_sharepoint_core::config::{Config, Library, expand_tilde};
use looper_sharepoint_core::config_edit::{self, AccountValues, NewLibrary};
use looper_sharepoint_core::setup::SetupFile;
use looper_sharepoint_graph::GraphClient;
use serde::Serialize;

use crate::links::{self, AppLink, Found};

/// What `setup` would do.
#[derive(Debug, Clone, Serialize)]
pub struct SetupPlan {
    pub account: Account,
    /// Libraries from the setup file that aren't configured yet.
    pub libraries: Vec<PlannedLibrary>,
    /// The organization and app this computer is set up for now, when they're different (needs `replace`).
    pub replaces: Option<Account>,
    #[serde(skip)]
    values: AccountValues,
    #[serde(skip)]
    new: Vec<NewLibrary>,
    #[serde(skip)]
    existing: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Account {
    pub tenant: String,
    pub client_id: String,
    pub app_name: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PlannedLibrary {
    pub site: String,
    pub library: String,
    pub folder: Option<String>,
    pub path: PathBuf,
}

/// The result of applying a setup plan.
#[derive(Debug, Clone, Serialize)]
pub struct SetupDone {
    #[serde(skip)]
    pub config: Config,
    /// Names of the libraries added.
    pub added: Vec<String>,
    /// Libraries from the file that couldn't be added, and why.
    pub skipped: Vec<String>,
    /// The organization or app changed: a running host must be restarted, and the user signs in again.
    pub switched: bool,
}

/// Work out what `setup` would do with `source`: a `looper-sharepoint://setup` link, or the path of a setup file.
pub fn plan_setup(config_path: &Path, source: &str) -> Result<SetupPlan> {
    let (values, new) = match links::parse_app_link(source)? {
        Some(AppLink::Setup { tenant, client_id, app_name, publisher }) => {
            (AccountValues { tenant, client_id, app_name, publisher, allowed_tenants: vec![] }, vec![])
        }
        Some(AppLink::Add { .. }) => bail!("that's a link to add a library, not a setup link (set up first, then add it)"),
        None => {
            if source.starts_with("http://") || source.starts_with("https://") {
                bail!("{source} is a web address: download the setup file first, then open it");
            }
            let p = expand_tilde(source);
            let text = std::fs::read_to_string(&p).with_context(|| format!("reading the setup file {}", p.display()))?;
            let f = SetupFile::parse(&text, &p).map_err(|e| anyhow::anyhow!("{e}"))?;
            (f.account_values(), f.new_libraries())
        }
    };
    let existing = if config_path.exists() { Some(std::fs::read_to_string(config_path)?) } else { None };
    let current = match &existing {
        Some(t) => Some(Config::parse(t, config_path).with_context(|| {
            format!("{} has a problem; fix it (or move it away) before setting up", config_path.display())
        })?),
        None => None,
    };
    let replaces = current
        .as_ref()
        .filter(|c| {
            !c.account.tenant.eq_ignore_ascii_case(&values.tenant) || !c.account.client_id.eq_ignore_ascii_case(&values.client_id)
        })
        .map(|c| Account { tenant: c.account.tenant.clone(), client_id: c.account.client_id.clone(), app_name: Some(c.account.app_name.clone()) });
    let new: Vec<NewLibrary> = new
        .into_iter()
        .filter(|l| !current.as_ref().is_some_and(|c| c.libraries.iter().any(|x| same(x, &l.site, &l.library, l.folder.as_deref()))))
        .collect();
    Ok(SetupPlan {
        account: Account { tenant: values.tenant.clone(), client_id: values.client_id.clone(), app_name: values.app_name.clone() },
        libraries: new
            .iter()
            .map(|l| PlannedLibrary { site: l.site.clone(), library: l.library.clone(), folder: l.folder.clone(), path: l.path.clone() })
            .collect(),
        replaces,
        values,
        new,
        existing,
    })
}

impl SetupPlan {
    /// Write the config. Switching to another organization or app needs `replace`.
    pub fn apply(&self, config_path: &Path, replace: bool) -> Result<SetupDone> {
        if let Some(r) = &self.replaces
            && !replace
        {
            bail!(
                "this computer is already set up for another organization or app (tenant {}, app {}). Switching keeps \
                 your libraries in the config, and you sign in again",
                r.tenant,
                r.client_id
            );
        }
        let (mut text, _) = config_edit::with_account(self.existing.as_deref(), config_path, &self.values).map_err(|e| anyhow::anyhow!("{e}"))?;
        let (mut added, mut skipped) = (vec![], vec![]);
        for l in &self.new {
            match config_edit::with_library(&text, config_path, l) {
                Ok((t, cfg)) => {
                    text = t;
                    added.push(cfg.libraries.last().map(|x| x.name().to_string()).unwrap_or_default());
                }
                Err(e) => skipped.push(format!("{} / {}: {e}", l.site, l.library)),
            }
        }
        config_edit::write(config_path, &text).with_context(|| format!("writing {}", config_path.display()))?;
        let config = Config::parse(&text, config_path)?;
        Ok(SetupDone { config, added, skipped, switched: self.replaces.is_some() })
    }
}

/// What `add` would do with a link.
#[derive(Debug, Clone, Serialize)]
pub struct AddPlan {
    pub site_url: String,
    pub site_name: String,
    pub library: String,
    pub folder: Option<String>,
    /// "folder \"Specs\" in Engineering / Documents"
    pub describe: String,
    pub other_libraries: Vec<String>,
    /// The local folder to suggest.
    pub suggested: PathBuf,
    /// A library that already syncs this, and its folder.
    pub already: Option<(String, PathBuf)>,
    #[serde(skip)]
    found: Found,
}

/// Find what `link` (a SharePoint link, or a `looper-sharepoint://add` link) points to, and what to suggest.
pub async fn plan_add(graph: &GraphClient, cfg: &Config, link: &str, library: Option<&str>) -> Result<AddPlan> {
    let (link, to) = match links::parse_app_link(link)? {
        Some(AppLink::Add { url, to }) => (url, to),
        Some(AppLink::Setup { .. }) => bail!("that's a setup link: set up with it first, then add the SharePoint link"),
        None => (link.trim().to_string(), None),
    };
    let found = links::find(graph, &link, library).await?;
    let already = cfg
        .libraries
        .iter()
        .find(|l| same(l, &found.site_url, &found.library, found.folder.as_deref()) || l.library == found.drive_id && same_folder(l, found.folder.as_deref()))
        .map(|l| (l.name().to_string(), cfg.local_root(l)));
    let suggested = match to {
        Some(t) => expand_tilde(&t),
        None => cfg.root().join(found.suggested_name()),
    };
    Ok(AddPlan {
        site_url: found.site_url.clone(),
        site_name: found.site_name.clone(),
        library: found.library.clone(),
        folder: found.folder.clone(),
        describe: found.describe(),
        other_libraries: found.other_libraries.clone(),
        suggested,
        already,
        found,
    })
}

/// How to add it.
#[derive(Debug, Clone, Default)]
pub struct AddOptions {
    pub to: PathBuf,
    pub name: Option<String>,
    pub read_only: bool,
    /// Allow a folder that already has files in it.
    pub adopt: bool,
}

impl AddPlan {
    /// Write the new `[[library]]` into the config; returns the new config and the library's name.
    pub fn apply(&self, config_path: &Path, o: &AddOptions) -> Result<(Config, String)> {
        let to = std::path::absolute(expand_tilde(&o.to.to_string_lossy()))?;
        if !o.adopt && has_files(&to) {
            bail!(
                "{} isn't empty. Choose an empty or new folder, or keep what's in it (\"adopt\"): files that match \
                 SharePoint's are kept, and files only in this folder are uploaded{}",
                to.display(),
                if o.read_only { " (read-only: kept, not uploaded)" } else { "" }
            );
        }
        let text = std::fs::read_to_string(config_path).with_context(|| format!("reading {}", config_path.display()))?;
        let new = NewLibrary {
            site: self.found.site_url.clone(),
            library: self.found.library.clone(),
            folder: self.found.folder.clone(),
            path: to,
            name: o.name.clone(),
            read_only: o.read_only,
        };
        let (text, cfg) = config_edit::with_library(&text, config_path, &new).map_err(|e| anyhow::anyhow!("not added: {e}"))?;
        config_edit::write(config_path, &text).with_context(|| format!("writing {}", config_path.display()))?;
        let name = cfg.libraries.last().map(|l| l.name().to_string()).unwrap_or_default();
        Ok((cfg, name))
    }
}

/// Take the library named `name` out of the config, and return it. Never touches files.
pub fn remove(config_path: &Path, name: &str) -> Result<(Config, Library)> {
    let text = std::fs::read_to_string(config_path).with_context(|| format!("reading {}", config_path.display()))?;
    let (text, lib) = config_edit::without_library(&text, config_path, name).map_err(|e| anyhow::anyhow!("{e}"))?;
    config_edit::write(config_path, &text).with_context(|| format!("writing {}", config_path.display()))?;
    Ok((Config::parse(&text, config_path)?, lib))
}

/// Forget a removed library's sync state (its state DB isn't the user's data), so adding it again starts fresh.
pub fn forget_state(data_dir: &Path, name: &str) -> Result<bool> {
    match crate::index::forget(data_dir, name)? {
        Some(e) => {
            for ext in ["", "-wal", "-shm"] {
                let _ = std::fs::remove_file(format!("{}{ext}", e.db_path.display()));
            }
            Ok(true)
        }
        None => Ok(false),
    }
}

fn same(l: &Library, site: &str, library: &str, folder: Option<&str>) -> bool {
    l.site.trim_end_matches('/').eq_ignore_ascii_case(site.trim_end_matches('/')) && l.library.eq_ignore_ascii_case(library) && same_folder(l, folder)
}

fn same_folder(l: &Library, folder: Option<&str>) -> bool {
    l.folder.as_deref().map(str::to_lowercase) == folder.map(str::to_lowercase)
}

fn has_files(dir: &Path) -> bool {
    std::fs::read_dir(dir).is_ok_and(|mut d| d.any(|e| e.is_ok_and(|e| e.file_name() != ".DS_Store")))
}
