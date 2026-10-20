//! Start at login: the app as a login item (`SMAppService.mainApp`, macOS 13+), or, with only the command line tool, a
//! LaunchAgent that runs `looper-sharepoint run --no-tui`. One switch: never both. `specs/onboarding-design.md` §8.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// The LaunchAgent's label (and plist name) for the command line tool.
pub const AGENT_LABEL: &str = "dev.looper-sharepoint.run";

/// The app's login item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AppItem {
    On,
    Off,
    /// Registered, but the user must allow it in System Settings → General → Login Items.
    NeedsApproval,
}

/// The `.app` this process runs from (`<App>.app/Contents/MacOS/<exe>`), if any.
pub fn app_bundle() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok().and_then(|e| std::fs::canonicalize(e).ok())?;
    let macos = exe.parent()?;
    let contents = macos.parent()?;
    let app = contents.parent()?;
    (macos.file_name()? == "MacOS" && contents.file_name()? == "Contents" && app.extension()? == "app").then(|| app.to_path_buf())
}

/// Why the app can't be a login item from where it is, if it can't.
pub fn app_unavailable() -> Option<String> {
    let Some(app) = app_bundle() else {
        return Some("this isn't the installed app (a development build)".into());
    };
    match crate::cli_link::location(&app) {
        crate::cli_link::Location::DiskImage => return Some("the app is running from its disk image; drag it to Applications first".into()),
        crate::cli_link::Location::Translocated => {
            return Some("macOS is running a temporary copy of the app; move it to Applications and open it from there".into());
        }
        crate::cli_link::Location::Dev => return Some("this is a development build".into()),
        crate::cli_link::Location::Installed => {}
    }
    #[cfg(target_os = "macos")]
    if objc2::runtime::AnyClass::get(c"SMAppService").is_none() {
        return Some("starting at login needs macOS 13 or later".into());
    }
    #[cfg(not(target_os = "macos"))]
    return Some("not supported on this platform yet".into());
    #[allow(unreachable_code)]
    None
}

#[cfg(target_os = "macos")]
pub fn app_status() -> AppItem {
    use objc2_service_management::{SMAppService, SMAppServiceStatus};
    if app_unavailable().is_some() {
        return AppItem::Off;
    }
    // SAFETY: plain calls on the main app's service object (macOS 13+, checked above).
    match unsafe { SMAppService::mainAppService().status() } {
        SMAppServiceStatus::Enabled => AppItem::On,
        SMAppServiceStatus::RequiresApproval => AppItem::NeedsApproval,
        _ => AppItem::Off,
    }
}

#[cfg(target_os = "macos")]
pub fn set_app(on: bool) -> Result<AppItem> {
    use objc2_service_management::SMAppService;
    if let Some(why) = app_unavailable() {
        bail!("can't start at login: {why}");
    }
    // SAFETY: as in `app_status`.
    let svc = unsafe { SMAppService::mainAppService() };
    let r = unsafe { if on { svc.registerAndReturnError() } else { svc.unregisterAndReturnError() } };
    if let Err(e) = r {
        // Unregistering something that isn't registered is fine.
        if on || app_status() != AppItem::Off {
            bail!("{}", e.localizedDescription());
        }
    }
    Ok(app_status())
}

/// System Settings → General → Login Items.
#[cfg(target_os = "macos")]
pub fn open_settings() {
    // SAFETY: opens a System Settings pane; no arguments.
    unsafe { objc2_service_management::SMAppService::openSystemSettingsLoginItems() };
}

#[cfg(not(target_os = "macos"))]
pub fn app_status() -> AppItem {
    AppItem::Off
}

#[cfg(not(target_os = "macos"))]
pub fn set_app(_on: bool) -> Result<AppItem> {
    bail!("starting at login isn't supported on this platform yet")
}

#[cfg(not(target_os = "macos"))]
pub fn open_settings() {}

/// `~/Library/LaunchAgents/dev.looper-sharepoint.run.plist`
pub fn agent_plist() -> Result<PathBuf> {
    Ok(dirs::home_dir().context("no home folder")?.join("Library/LaunchAgents").join(format!("{AGENT_LABEL}.plist")))
}

pub fn agent_installed() -> bool {
    agent_plist().is_ok_and(|p| p.exists())
}

/// The LaunchAgent: at login, run `<exe> --config <config> run --no-tui`, logging to the data folder.
pub fn agent_text(exe: &Path, config: &Path, data_dir: &Path) -> String {
    let x = |p: &Path| p.to_string_lossy().replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    let log = data_dir.join("logs").join("run-at-login.log");
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<!-- Made by `looper-sharepoint start-at-login on`; removed by `looper-sharepoint start-at-login off`. -->
<plist version="1.0">
<dict>
  <key>Label</key><string>{AGENT_LABEL}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{}</string>
    <string>--config</string><string>{}</string>
    <string>run</string><string>--no-tui</string>
  </array>
  <key>RunAtLoad</key><true/>
  <key>StandardOutPath</key><string>{}</string>
  <key>StandardErrorPath</key><string>{}</string>
</dict>
</plist>
"#,
        x(exe),
        x(config),
        x(&log),
        x(&log)
    )
}

pub fn install_agent(exe: &Path, config: &Path, data_dir: &Path) -> Result<PathBuf> {
    let p = agent_plist()?;
    std::fs::create_dir_all(p.parent().unwrap())?;
    std::fs::create_dir_all(data_dir.join("logs"))?;
    std::fs::write(&p, agent_text(exe, config, data_dir)).with_context(|| format!("writing {}", p.display()))?;
    Ok(p)
}

/// Remove the LaunchAgent (only ours: it carries our label). Doesn't stop a host it already started.
pub fn remove_agent() -> Result<bool> {
    let p = agent_plist()?;
    match std::fs::read_to_string(&p) {
        Ok(t) if t.contains(AGENT_LABEL) => {
            std::fs::remove_file(&p)?;
            Ok(true)
        }
        Ok(_) => bail!("{} isn't looper-sharepoint's; left as it is", p.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// What the tray remembers about starting at login: it's turned on once, after the first sync, unless the user has
/// turned it off. `<data_dir>/login-item.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Prefs {
    /// When the app turned it on by itself (it only ever does that once).
    #[serde(default)]
    pub auto_enabled_at: Option<String>,
    /// The user turned it off in the app: never turn it on again by itself.
    #[serde(default)]
    pub user_turned_off: bool,
}

fn prefs_file(data_dir: &Path) -> PathBuf {
    data_dir.join("login-item.json")
}

pub fn load_prefs(data_dir: &Path) -> Prefs {
    std::fs::read_to_string(prefs_file(data_dir)).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

pub fn save_prefs(data_dir: &Path, p: &Prefs) -> Result<()> {
    std::fs::create_dir_all(data_dir)?;
    std::fs::write(prefs_file(data_dir), serde_json::to_string_pretty(p)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_plist_runs_the_host_with_this_config() {
        let t = agent_text(Path::new("/opt/x/looper-sharepoint"), Path::new("/Users/a/c&d.toml"), Path::new("/Users/a/data"));
        assert!(t.contains("<string>/opt/x/looper-sharepoint</string>"));
        assert!(t.contains("<string>--config</string><string>/Users/a/c&amp;d.toml</string>"), "{t}");
        assert!(t.contains("<string>run</string><string>--no-tui</string>"));
        assert!(t.contains(AGENT_LABEL) && t.contains("<key>RunAtLoad</key><true/>"));
    }

    #[test]
    fn prefs_round_trip() {
        let d = tempfile::tempdir().unwrap();
        assert!(!load_prefs(d.path()).user_turned_off);
        save_prefs(d.path(), &Prefs { auto_enabled_at: Some("now".into()), user_turned_off: true }).unwrap();
        let p = load_prefs(d.path());
        assert!(p.user_turned_off && p.auto_enabled_at.as_deref() == Some("now"));
    }
}
