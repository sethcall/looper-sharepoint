//! `config.toml`: the user's configuration. See `specs/m1-design.md` §6.
//!
//! Unknown keys are rejected: this tool can delete files, so a typo must not silently fall back to a default.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

use crate::naming::{CharMap, NamingMode};

/// Folder under `sync.root` for local edits that two-way sync set aside instead of uploading.
pub const SET_ASIDE_DIR: &str = "not-uploaded";

pub const DEFAULT_SCOPES: &[&str] = &[
    "Files.ReadWrite.All",
    "Sites.Read.All",
    "offline_access",
    "User.Read",
    "openid",
    "profile",
];

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    // The cause is a plain field rather than `#[source]`: it's already in the message, and `{:#}` would print it twice.
    #[error("reading {path}: {reason}")]
    Read { path: PathBuf, reason: std::io::Error },
    #[error("parsing {path}: {reason}")]
    Parse { path: PathBuf, reason: toml::de::Error },
    #[error("invalid config: {0}")]
    Invalid(String),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub account: Account,
    #[serde(default)]
    pub sync: Sync,
    #[serde(default)]
    pub naming: Naming,
    #[serde(default)]
    pub network: Network,
    #[serde(default, rename = "library")]
    pub libraries: Vec<Library>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Account {
    /// Tenant GUID or domain (e.g. `contoso.onmicrosoft.com`).
    pub tenant: String,
    /// Application (client) ID of the app registration.
    pub client_id: String,
    #[serde(default)]
    pub auth_flow: AuthFlow,
    #[serde(default = "default_scopes")]
    pub scopes: Vec<String>,
    /// Validated in M1, enforced by policy later.
    #[serde(default)]
    pub allowed_tenants: Vec<String>,
    /// Product name shown to users (sign-in page, CLI, tray) and sent in the User-Agent. Set it to the app
    /// registration's display name, so what users see matches what Entra shows.
    #[serde(default = "default_name")]
    pub app_name: String,
    /// Organisation in the User-Agent (`ISV|<publisher>|<app_name>/<version>`).
    #[serde(default = "default_name")]
    pub publisher: String,
}

fn default_name() -> String {
    "looper-sharepoint".into()
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Network {
    /// Minimum TLS version for every connection. `"1.2"` is only for networks whose TLS-inspecting proxy can't do 1.3.
    #[serde(default)]
    pub min_tls: MinTls,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
pub enum MinTls {
    #[serde(rename = "1.2")]
    Tls12,
    #[default]
    #[serde(rename = "1.3")]
    Tls13,
}

fn default_scopes() -> Vec<String> {
    DEFAULT_SCOPES.iter().map(|s| s.to_string()).collect()
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AuthFlow {
    #[default]
    Pkce,
    DeviceCode,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sync {
    #[serde(default = "default_root")]
    pub root: String,
    #[serde(default)]
    pub direction: Direction,
    #[serde(default)]
    pub on_remote_delete: OnRemoteDelete,
    #[serde(default)]
    pub local_orphans: LocalOrphans,
    #[serde(default = "default_max_delete_fraction")]
    pub max_delete_fraction: f64,
    #[serde(default = "default_parallel")]
    pub max_parallel_downloads: usize,
    /// How often the service checks each library for remote changes while it isn't receiving live updates.
    #[serde(default = "default_poll_interval", with = "humantime_serde")]
    pub poll_interval: Duration,
    /// Receive live change notifications (Graph socket.io push). Off = poll only.
    #[serde(default = "default_true")]
    pub push: bool,
    /// While live updates are connected, still check this often in case a notification was missed.
    #[serde(default = "default_push_safety_interval", with = "humantime_serde")]
    pub push_safety_interval: Duration,
    /// Watch library folders and sync shortly after local changes (two-way only).
    #[serde(default = "default_true")]
    pub watch: bool,
    /// Extra glob patterns (matched against each file or folder name, and against the path relative to the library
    /// folder) for local files that are never uploaded. Added to [`crate::ignore::BUILTIN`].
    #[serde(default)]
    pub ignore: Vec<String>,
    /// Client-side request pacing, below SharePoint's per-user limit (`docs/sharepoint/throttling.md`).
    #[serde(default = "default_rps")]
    pub max_requests_per_second: u32,
}

impl Default for Sync {
    fn default() -> Self {
        Self {
            root: default_root(),
            direction: Direction::default(),
            on_remote_delete: OnRemoteDelete::default(),
            local_orphans: LocalOrphans::default(),
            max_delete_fraction: default_max_delete_fraction(),
            max_parallel_downloads: default_parallel(),
            poll_interval: default_poll_interval(),
            push: true,
            push_safety_interval: default_push_safety_interval(),
            watch: true,
            ignore: vec![],
            max_requests_per_second: default_rps(),
        }
    }
}

fn default_root() -> String {
    "~/SharePoint".into()
}
fn default_max_delete_fraction() -> f64 {
    0.25
}
fn default_parallel() -> usize {
    4
}
fn default_poll_interval() -> Duration {
    Duration::from_secs(45)
}
fn default_true() -> bool {
    true
}
fn default_push_safety_interval() -> Duration {
    Duration::from_secs(15 * 60)
}
pub const MIN_PUSH_SAFETY_INTERVAL: Duration = Duration::from_secs(60);
fn default_rps() -> u32 {
    8
}
pub const MIN_POLL_INTERVAL: Duration = Duration::from_secs(15);

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    DownloadOnly,
    #[default]
    TwoWay,
}

/// What to do with a local copy when its item was deleted in SharePoint.
/// Recovery is via the SharePoint recycle bin; the client keeps no local trash.
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OnRemoteDelete {
    #[default]
    Delete,
    Keep,
}

/// What to do with local files that were never seen remotely.
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LocalOrphans {
    #[default]
    Keep,
    Delete,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Naming {
    #[serde(default)]
    pub mode: NamingMode,
    /// Overrides/extensions to the default remote→local character map.
    #[serde(default)]
    pub char_map: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Library {
    /// Site URL, e.g. `https://contoso.sharepoint.com/sites/Engineering`.
    pub site: String,
    /// Library display name or drive id.
    pub library: String,
    /// Folder under `sync.root`.
    pub local: String,
    #[serde(default)]
    pub include: Vec<String>,
    #[serde(default)]
    pub exclude: Vec<String>,
    #[serde(default)]
    pub read_only: bool,
}

impl Config {
    pub fn default_path() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("looper-sharepoint")
            .join("config.toml")
    }

    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|reason| ConfigError::Read {
            path: path.to_owned(),
            reason,
        })?;
        Self::parse(&text, path)
    }

    pub fn parse(text: &str, path: &Path) -> Result<Self, ConfigError> {
        let cfg: Config = toml::from_str(text).map_err(|reason| ConfigError::Parse {
            path: path.to_owned(),
            reason,
        })?;
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        let invalid = |m: String| Err(ConfigError::Invalid(m));
        if self.account.client_id.trim_matches(['0', '-']).is_empty() || self.account.tenant.eq_ignore_ascii_case("contoso.onmicrosoft.com") {
            return invalid(
                "account.tenant and account.client_id are still the example values; set them to your organization's \
                 app registration (ask your Entra admin)"
                    .into(),
            );
        }
        if self.account.app_name.trim().is_empty() || self.account.publisher.trim().is_empty() {
            return invalid("account.app_name and account.publisher can't be empty".into());
        }
        for g in &self.sync.ignore {
            globset::Glob::new(g).map_err(|e| ConfigError::Invalid(format!("bad sync.ignore glob {g:?}: {e}")))?;
        }
        if !(0.0..=1.0).contains(&self.sync.max_delete_fraction) {
            return invalid("sync.max_delete_fraction must be between 0.0 and 1.0".into());
        }
        if self.sync.poll_interval < MIN_POLL_INTERVAL {
            return invalid(format!("sync.poll_interval must be at least {}s", MIN_POLL_INTERVAL.as_secs()));
        }
        if self.sync.push_safety_interval < MIN_PUSH_SAFETY_INTERVAL {
            return invalid(format!("sync.push_safety_interval must be at least {}s", MIN_PUSH_SAFETY_INTERVAL.as_secs()));
        }
        if !(1..=10).contains(&self.sync.max_requests_per_second) {
            return invalid("sync.max_requests_per_second must be between 1 and 10".into());
        }
        if self.sync.max_parallel_downloads == 0 {
            return invalid("sync.max_parallel_downloads must be at least 1".into());
        }
        if !self.account.allowed_tenants.is_empty()
            && !self
                .account
                .allowed_tenants
                .iter()
                .any(|t| t.eq_ignore_ascii_case(&self.account.tenant))
        {
            return invalid(format!(
                "account.tenant {:?} is not in account.allowed_tenants",
                self.account.tenant
            ));
        }
        let mut locals = std::collections::HashSet::new();
        for lib in &self.libraries {
            if lib.local.is_empty() || lib.local.contains("..") || Path::new(&lib.local).is_absolute() {
                return invalid(format!("library.local {:?} must be a relative folder name", lib.local));
            }
            if !locals.insert(lib.local.to_lowercase()) {
                return invalid(format!("library.local {:?} is used twice", lib.local));
            }
            if lib.local.split(['/', '\\']).next().is_some_and(|first| first.eq_ignore_ascii_case(SET_ASIDE_DIR)) {
                return invalid(format!("library.local {:?}: {SET_ASIDE_DIR:?} is reserved for files set aside by sync", lib.local));
            }
            for g in lib.include.iter().chain(&lib.exclude) {
                globset::Glob::new(g).map_err(|e| ConfigError::Invalid(format!("bad glob {g:?}: {e}")))?;
            }
        }
        self.char_map()?;
        Ok(())
    }

    pub fn char_map(&self) -> Result<CharMap, ConfigError> {
        CharMap::with_overrides(&self.naming.char_map).map_err(ConfigError::Invalid)
    }

    /// `sync.root` with `~` expanded.
    /// Where two-way sync moves local edits it must not upload (their folder vanished from SharePoint):
    /// `<root>/not-uploaded/<library>/`.
    pub fn set_aside_dir(&self, lib: &Library) -> PathBuf {
        self.root().join(SET_ASIDE_DIR).join(&lib.local)
    }

    pub fn root(&self) -> PathBuf {
        expand_tilde(&self.sync.root)
    }

    /// Whether local changes in `lib` are uploaded: two-way sync, and the library isn't read-only.
    pub fn uploads(&self, lib: &Library) -> bool {
        self.sync.direction == Direction::TwoWay && !lib.read_only
    }

    pub fn library(&self, local: &str) -> Option<&Library> {
        self.libraries.iter().find(|l| l.local.eq_ignore_ascii_case(local))
    }
}

pub fn expand_tilde(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/").or_else(|| p.strip_prefix("~\\")) {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    } else if p == "~"
        && let Some(home) = dirs::home_dir() {
            return home;
        }
    PathBuf::from(p)
}

/// Where the client keeps its own state: `<data_dir>/looper-sharepoint`. Never inside the sync root.
/// `LOOPER_SHAREPOINT_DATA_DIR` overrides it (tests, multiple isolated setups).
pub fn data_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("LOOPER_SHAREPOINT_DATA_DIR") {
        return PathBuf::from(d);
    }
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("looper-sharepoint")
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: &str = r#"
[account]
tenant = "example.onmicrosoft.com"
client_id = "11111111-2222-3333-4444-555555555555"
"#;

    fn parse(s: &str) -> Result<Config, ConfigError> {
        Config::parse(s, Path::new("test.toml"))
    }

    #[test]
    fn identity_and_tls() {
        let c = parse(MIN).unwrap();
        assert_eq!(c.account.app_name, "looper-sharepoint");
        assert_eq!(c.network.min_tls, MinTls::Tls13);
        let c = parse(&format!("{MIN}app_name = \"Example Sync\"\npublisher = \"Example\"\n[network]\nmin_tls = \"1.2\"\n")).unwrap();
        assert_eq!(c.account.app_name, "Example Sync");
        assert_eq!(c.network.min_tls, MinTls::Tls12);
        assert!(parse(&format!("{MIN}[network]\nmin_tls = \"1.1\"\n")).is_err());
        // The example file's placeholders are refused, so an unedited config never runs.
        let example = include_str!("../../../config.example.toml");
        assert!(parse(example).unwrap_err().to_string().contains("example values"));
    }

    #[test]
    fn defaults() {
        let c = parse(MIN).unwrap();
        assert_eq!(c.sync.on_remote_delete, OnRemoteDelete::Delete);
        assert_eq!(c.sync.local_orphans, LocalOrphans::Keep);
        assert_eq!(c.sync.max_delete_fraction, 0.25);
        assert_eq!(c.account.auth_flow, AuthFlow::Pkce);
        assert_eq!(c.sync.poll_interval, Duration::from_secs(45));
        assert_eq!(c.sync.max_requests_per_second, 8);
        assert!(c.account.scopes.contains(&"offline_access".to_string()));
    }

    #[test]
    fn rejects_unknown_keys() {
        let e = parse(&format!("{MIN}\n[sync]\nroot = \"x\"\non_remote_delte = \"keep\"\n")).unwrap_err();
        assert!(e.to_string().contains("on_remote_delte"), "{e}");
    }

    #[test]
    fn poll_interval() {
        let c = parse(&format!("{MIN}\n[sync]\npoll_interval = \"2m\"\n")).unwrap();
        assert_eq!(c.sync.poll_interval, Duration::from_secs(120));
        assert!(parse(&format!("{MIN}\n[sync]\npoll_interval = \"5s\"\n")).is_err());
    }

    #[test]
    fn push_settings() {
        let c = parse(MIN).unwrap();
        assert!(c.sync.push);
        assert_eq!(c.sync.push_safety_interval, Duration::from_secs(900));
        let c = parse(&format!("{MIN}\n[sync]\npush = false\npush_safety_interval = \"1h\"\n")).unwrap();
        assert!(!c.sync.push);
        assert_eq!(c.sync.push_safety_interval, Duration::from_secs(3600));
        assert!(parse(&format!("{MIN}\n[sync]\npush_safety_interval = \"30s\"\n")).is_err());
        assert!(parse(&format!("{MIN}\n[sync]\nmax_requests_per_second = 20\n")).is_err());
    }

    #[test]
    fn direction_and_read_only() {
        let c = parse(MIN).unwrap();
        assert_eq!(c.sync.direction, Direction::TwoWay);
        assert!(c.sync.watch);
        let c = parse(&format!("{MIN}\n[sync]\ndirection = \"download_only\"\nignore = [\"*.bak\"]\n")).unwrap();
        assert_eq!(c.sync.direction, Direction::DownloadOnly);
        assert!(parse(&format!("{MIN}\n[sync]\nignore = [\"[\"]\n")).is_err());
    }

    #[test]
    fn libraries_and_duplicates() {
        let lib = r#"
[[library]]
site = "https://contoso.sharepoint.com/sites/eng"
library = "Documents"
local = "eng"
include = ["specs/**"]
"#;
        let c = parse(&format!("{MIN}{lib}")).unwrap();
        assert_eq!(c.libraries[0].include, vec!["specs/**"]);
        assert!(parse(&format!("{MIN}{lib}{}", lib.replace("eng\"\ninclude", "ENG\"\ninclude"))).is_err());
        assert!(parse(&format!("{MIN}{}", lib.replace("local = \"eng\"", "local = \"../x\""))).is_err());
    }

    #[test]
    fn allowed_tenants() {
        assert!(parse(&MIN.replace("[account]", "[account]\nallowed_tenants = [\"other.onmicrosoft.com\"]")).is_err());
        assert!(parse(&MIN.replace("[account]", "[account]\nallowed_tenants = [\"EXAMPLE.onmicrosoft.com\"]")).is_ok());
    }
}
