//! Setup files (`<name>.looper-sharepoint`): an organization's values, and optionally libraries, for
//! `looper-sharepoint setup`. See `specs/onboarding-design.md` §4.
//!
//! A setup file can only name the organization and app, and libraries to add. Anything else is refused by name, so a
//! setup file never changes how sync behaves (deletes, direction, ...).

use std::path::Path;

use serde::Deserialize;
use toml_edit::{Array, DocumentMut, Item, Table, value};

use crate::config::{Config, ConfigError};
use crate::config_edit::{AccountValues, NewLibrary};

/// The extension of setup files.
pub const EXTENSION: &str = "looper-sharepoint";

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetupFile {
    pub account: SetupAccount,
    #[serde(default, rename = "library")]
    pub libraries: Vec<SetupLibrary>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetupAccount {
    pub tenant: String,
    pub client_id: String,
    #[serde(default)]
    pub app_name: Option<String>,
    #[serde(default)]
    pub publisher: Option<String>,
    #[serde(default)]
    pub allowed_tenants: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetupLibrary {
    pub site: String,
    pub library: String,
    #[serde(default)]
    pub folder: Option<String>,
    /// The local folder (`~/` allowed).
    pub path: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub read_only: bool,
}

impl SetupFile {
    pub fn parse(text: &str, path: &Path) -> Result<Self, ConfigError> {
        let f: SetupFile = toml::from_str(text).map_err(|reason| ConfigError::Parse { path: path.to_owned(), reason })?;
        if f.account.tenant.trim().is_empty() || f.account.client_id.trim().is_empty() {
            return Err(ConfigError::Invalid(format!("{}: the setup file needs account.tenant and account.client_id", path.display())));
        }
        Ok(f)
    }

    pub fn account_values(&self) -> AccountValues {
        AccountValues {
            tenant: self.account.tenant.clone(),
            client_id: self.account.client_id.clone(),
            app_name: self.account.app_name.clone(),
            publisher: self.account.publisher.clone(),
            allowed_tenants: self.account.allowed_tenants.clone(),
        }
    }

    pub fn new_libraries(&self) -> Vec<NewLibrary> {
        self.libraries
            .iter()
            .map(|l| NewLibrary {
                site: l.site.clone(),
                library: l.library.clone(),
                folder: l.folder.clone(),
                path: crate::config::expand_tilde(&l.path),
                name: l.name.clone(),
                read_only: l.read_only,
            })
            .collect()
    }
}

/// A setup file for `cfg`'s organization and app, plus its libraries when `with_libraries`.
pub fn export(cfg: &Config, with_libraries: bool) -> String {
    let default = |s: &str| s == "looper-sharepoint";
    let mut doc = DocumentMut::new();
    let mut a = Table::new();
    a.insert("tenant", value(&cfg.account.tenant));
    a.insert("client_id", value(&cfg.account.client_id));
    if !default(&cfg.account.app_name) {
        a.insert("app_name", value(&cfg.account.app_name));
    }
    if !default(&cfg.account.publisher) {
        a.insert("publisher", value(&cfg.account.publisher));
    }
    if !cfg.account.allowed_tenants.is_empty() {
        a.insert("allowed_tenants", value(cfg.account.allowed_tenants.iter().collect::<Array>()));
    }
    doc.insert("account", Item::Table(a));
    if with_libraries && !cfg.libraries.is_empty() {
        let mut libs = toml_edit::ArrayOfTables::new();
        for l in &cfg.libraries {
            let mut t = Table::new();
            t.insert("site", value(&l.site));
            t.insert("library", value(&l.library));
            if let Some(f) = &l.folder {
                t.insert("folder", value(f));
            }
            t.insert("path", value(tilde(&cfg.local_root(l))));
            t.insert("name", value(l.name()));
            if l.read_only {
                t.insert("read_only", value(true));
            }
            libs.push(t);
        }
        doc.insert("library", Item::ArrayOfTables(libs));
    }
    let name = if default(&cfg.account.app_name) { "looper-sharepoint".to_string() } else { cfg.account.app_name.clone() };
    format!(
        "# {name}: setup for looper-sharepoint. Open this file with Looper SharePoint, or run\n\
         #   looper-sharepoint setup <this file>\n\
         # It names your organization and its app registration; it holds no passwords or other secrets.\n\n{doc}"
    )
}

fn tilde(p: &Path) -> String {
    match dirs::home_dir().and_then(|h| p.strip_prefix(&h).ok().map(Path::to_path_buf)) {
        Some(rest) if !rest.as_os_str().is_empty() => format!("~/{}", rest.to_string_lossy().replace('\\', "/")),
        _ => p.to_string_lossy().into_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CFG: &str = r#"
[account]
tenant = "11111111-2222-3333-4444-555555555555"
client_id = "66666666-7777-8888-9999-000000000000"
app_name = "Contoso Sync"
[sync]
direction = "download_only"
[[library]]
site = "https://contoso.sharepoint.com/sites/eng"
library = "Documents"
folder = "Specs"
path = "/work/specs"
read_only = true
"#;

    #[test]
    fn export_and_parse_round_trip() {
        let cfg = Config::parse(CFG, Path::new("c.toml")).unwrap();
        let text = export(&cfg, true);
        assert!(text.starts_with("# Contoso Sync: setup for looper-sharepoint."), "{text}");
        assert!(!text.contains("direction"), "sync settings never travel: {text}");
        let f = SetupFile::parse(&text, Path::new("s")).unwrap();
        assert_eq!(f.account.app_name.as_deref(), Some("Contoso Sync"));
        assert_eq!(f.account.publisher, None);
        let libs = f.new_libraries();
        assert_eq!((libs[0].folder.as_deref(), libs[0].read_only), (Some("Specs"), true));
        assert_eq!(libs[0].path, std::path::PathBuf::from("/work/specs"));
        // Without libraries.
        assert!(SetupFile::parse(&export(&cfg, false), Path::new("s")).unwrap().libraries.is_empty());
    }

    #[test]
    fn only_allowed_keys() {
        let base = "[account]\ntenant = \"t\"\nclient_id = \"c\"\n";
        assert!(SetupFile::parse(base, Path::new("s")).is_ok());
        let e = SetupFile::parse(&format!("{base}[sync]\ndirection = \"download_only\"\n"), Path::new("s")).unwrap_err();
        assert!(e.to_string().contains("sync"), "{e}");
        let e = SetupFile::parse(&format!("{base}scopes = [\"x\"]\n"), Path::new("s")).unwrap_err();
        assert!(e.to_string().contains("scopes"), "{e}");
        assert!(SetupFile::parse("[account]\ntenant = \"\"\nclient_id = \"c\"\n", Path::new("s")).is_err());
    }
}
