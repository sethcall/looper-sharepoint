//! Changing `config.toml` for the user (`add`, `remove`, `setup`), keeping their comments and order. Every change is
//! checked by parsing the result as a [`Config`] before anything is written, so a change that would make the config
//! invalid (an overlapping folder, a duplicate name) is refused with the same message `config check` would give.

use std::path::Path;

use toml_edit::{Array, DocumentMut, Item, Table, value};

use crate::config::{Config, ConfigError, Library};

/// A library to add.
#[derive(Debug, Clone, Default)]
pub struct NewLibrary {
    pub site: String,
    pub library: String,
    pub folder: Option<String>,
    /// The local folder, absolute (written with `~/` when it's in the home folder).
    pub path: std::path::PathBuf,
    /// Only written when it differs from the default (the last part of `path`).
    pub name: Option<String>,
    pub read_only: bool,
}

/// The organization's values, for a new config.
#[derive(Debug, Clone, Default)]
pub struct AccountValues {
    pub tenant: String,
    pub client_id: String,
    pub app_name: Option<String>,
    pub publisher: Option<String>,
    pub allowed_tenants: Vec<String>,
}

fn parse_doc(text: &str, path: &Path) -> Result<DocumentMut, ConfigError> {
    text.parse::<DocumentMut>().map_err(|e| ConfigError::Invalid(format!("{}: {e}", path.display())))
}

/// `text` with `lib` added as a new `[[library]]`, and the config it gives.
pub fn with_library(text: &str, path: &Path, lib: &NewLibrary) -> Result<(String, Config), ConfigError> {
    let mut doc = parse_doc(text, path)?;
    let mut t = Table::new();
    t.insert("site", value(&lib.site));
    t.insert("library", value(&lib.library));
    if let Some(f) = &lib.folder {
        t.insert("folder", value(f));
    }
    t.insert("path", value(tilde(&lib.path)));
    if let Some(n) = &lib.name {
        t.insert("name", value(n));
    }
    if lib.read_only {
        t.insert("read_only", value(true));
    }
    match doc.entry("library").or_insert(Item::ArrayOfTables(Default::default())) {
        Item::ArrayOfTables(a) => a.push(t),
        _ => return Err(ConfigError::Invalid("`library` in the config isn't a list of [[library]] blocks".into())),
    }
    let out = doc.to_string();
    let cfg = Config::parse(&out, path)?;
    Ok((out, cfg))
}

/// `text` without the library named `name`, and the library removed.
pub fn without_library(text: &str, path: &Path, name: &str) -> Result<(String, Library), ConfigError> {
    let cfg = Config::parse(text, path)?;
    let i = cfg
        .libraries
        .iter()
        .position(|l| l.name().eq_ignore_ascii_case(name))
        .ok_or_else(|| ConfigError::Invalid(format!("no library named {name:?} (`looper-sharepoint status` lists them)")))?;
    let mut doc = parse_doc(text, path)?;
    match doc.get_mut("library") {
        Some(Item::ArrayOfTables(a)) if a.len() == cfg.libraries.len() => {
            a.remove(i);
        }
        _ => return Err(ConfigError::Invalid("the config's [[library]] blocks can't be edited automatically; edit the file".into())),
    }
    if doc.get("library").and_then(Item::as_array_of_tables).is_some_and(|a| a.is_empty()) {
        doc.remove("library");
    }
    let out = doc.to_string();
    Config::parse(&out, path)?;
    Ok((out, cfg.libraries[i].clone()))
}

/// `text` (or a new config, when `None`) with `[account]` set to `account`, and the config it gives.
pub fn with_account(text: Option<&str>, path: &Path, account: &AccountValues) -> Result<(String, Config), ConfigError> {
    let mut doc = match text {
        Some(t) => parse_doc(t, path)?,
        None => parse_doc(NEW_CONFIG, path)?,
    };
    let a = doc.entry("account").or_insert(Item::Table(Table::new()));
    let a = a.as_table_mut().ok_or_else(|| ConfigError::Invalid("`account` in the config isn't a table".into()))?;
    a.insert("tenant", value(&account.tenant));
    a.insert("client_id", value(&account.client_id));
    for (k, v) in [("app_name", &account.app_name), ("publisher", &account.publisher)] {
        if let Some(v) = v {
            a.insert(k, value(v));
        }
    }
    if !account.allowed_tenants.is_empty() {
        a.insert("allowed_tenants", value(account.allowed_tenants.iter().collect::<Array>()));
    }
    let out = doc.to_string();
    let cfg = Config::parse(&out, path)?;
    Ok((out, cfg))
}

/// The start of a config made by `setup`.
const NEW_CONFIG: &str = "# looper-sharepoint configuration, made by `looper-sharepoint setup`. Libraries are added with\n\
# `looper-sharepoint add <SharePoint link>`. All settings: config.example.toml in the source, or `docs/user-guide.md`.\n\n\
[account]\n";

/// Write `text` to `path` through a temporary file in the same folder, so a crash never leaves half a config.
pub fn write(path: &Path, text: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)
}

/// `~/…` for a path in the home folder, as the user would write it.
fn tilde(p: &Path) -> String {
    match dirs::home_dir().and_then(|h| p.strip_prefix(&h).ok().map(Path::to_path_buf)) {
        Some(rest) if !rest.as_os_str().is_empty() => format!("~/{}", rest.to_string_lossy().replace('\\', "/")),
        _ => p.to_string_lossy().into_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = r#"# my settings
[account]
tenant = "example.onmicrosoft.com"   # ours
client_id = "11111111-2222-3333-4444-555555555555"

[sync]
root = "/tmp/sp"

# the old one
[[library]]
site = "https://contoso.sharepoint.com/sites/eng"
library = "Documents"
local = "eng"
"#;

    fn lib(path: &str, folder: Option<&str>) -> NewLibrary {
        NewLibrary {
            site: "https://contoso.sharepoint.com/sites/ops".into(),
            library: "Documents".into(),
            folder: folder.map(Into::into),
            path: path.into(),
            ..Default::default()
        }
    }

    #[test]
    fn add_keeps_comments_and_checks() {
        let p = Path::new("c.toml");
        let (text, cfg) = with_library(BASE, p, &lib("/work/ops", Some("Team/Plans"))).unwrap();
        assert!(text.starts_with("# my settings\n[account]\ntenant = \"example.onmicrosoft.com\"   # ours\n"), "{text}");
        assert!(text.contains("# the old one\n[[library]]"), "{text}");
        assert_eq!(cfg.libraries.len(), 2);
        assert_eq!((cfg.libraries[1].name(), cfg.libraries[1].folder.as_deref()), ("ops", Some("Team/Plans")));
        // Inside another library's folder: refused, with the config check's reason.
        let e = with_library(BASE, p, &lib("/tmp/sp/eng/inner", None)).unwrap_err();
        assert!(e.to_string().contains("inside the other"), "{e}");
        // In the home folder: written with ~/.
        let home = dirs::home_dir().unwrap();
        let (text, _) = with_library(BASE, p, &lib(&home.join("work/ops").to_string_lossy(), None)).unwrap();
        assert!(text.contains("path = \"~/work/ops\""), "{text}");
    }

    #[test]
    fn remove_by_name() {
        let p = Path::new("c.toml");
        let (text, _) = with_library(BASE, p, &lib("/work/ops", None)).unwrap();
        let (text, gone) = without_library(&text, p, "OPS").unwrap();
        assert_eq!(gone.site, "https://contoso.sharepoint.com/sites/ops");
        assert!(text.contains("local = \"eng\"") && !text.contains("/work/ops"), "{text}");
        let (text, _) = without_library(&text, p, "eng").unwrap();
        assert!(!text.contains("[[library]]") && text.contains("# my settings"), "{text}");
        assert!(without_library(&text, p, "eng").is_err());
    }

    #[test]
    fn account_new_and_existing() {
        let p = Path::new("c.toml");
        let acct = AccountValues {
            tenant: "22222222-2222-2222-2222-222222222222".into(),
            client_id: "33333333-3333-3333-3333-333333333333".into(),
            app_name: Some("Contoso Sync".into()),
            ..Default::default()
        };
        let (text, cfg) = with_account(None, p, &acct).unwrap();
        assert_eq!(cfg.account.app_name, "Contoso Sync");
        assert!(text.starts_with("# looper-sharepoint configuration"), "{text}");
        let (text, cfg) = with_account(Some(BASE), p, &acct).unwrap();
        assert_eq!(cfg.account.tenant, acct.tenant);
        assert!(text.contains("local = \"eng\""));
    }
}
