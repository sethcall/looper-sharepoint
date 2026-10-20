//! `setup`, `add` and `remove`: the terminal side of `looper_sharepoint_engine::onboard`. See
//! `specs/onboarding-design.md` §4–§5.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use looper_sharepoint_engine::core::config::{AuthFlow, Config};
use looper_sharepoint_engine::links;
use looper_sharepoint_engine::onboard::{self, AddOptions};
use looper_sharepoint_engine::RunOptions;
use looper_sharepoint_ipc::{Request, Response};
use serde_json::json;

pub struct AddArgs {
    pub link: String,
    pub to: Option<PathBuf>,
    pub library: Option<String>,
    pub name: Option<String>,
    pub read_only: bool,
    pub no_sync: bool,
    pub adopt: bool,
    pub yes: bool,
}

pub async fn add(config_path: &Path, data: &Path, a: AddArgs, as_json: bool) -> Result<ExitCode> {
    let cfg = load(config_path)?;
    let graph = signed_in_graph(&cfg, data).await?;
    let plan = onboard::plan_add(&graph, &cfg, &a.link, a.library.as_deref()).await?;
    if let Some((name, path)) = &plan.already {
        if as_json {
            println!("{}", json!({ "added": false, "already": name, "path": path }));
        } else {
            println!("Already syncing {} to {} (library {name:?}).", plan.describe, path.display());
        }
        return Ok(ExitCode::SUCCESS);
    }
    let to = match &a.to {
        Some(to) => to.clone(),
        None if !a.yes && !as_json && std::io::IsTerminal::is_terminal(&std::io::stdin()) => {
            PathBuf::from(super::prompt("Local folder", &tilde(&plan.suggested))?)
        }
        None => plan.suggested.clone(),
    };
    let opts = AddOptions { to, name: a.name.clone(), read_only: a.read_only, adopt: a.adopt };
    let (new_cfg, name) = plan.apply(config_path, &opts)?;
    let path = new_cfg.library(&name).map(|l| new_cfg.local_root(l)).unwrap_or_default();
    if as_json {
        println!("{}", json!({ "added": true, "name": name, "path": path, "site": plan.site_url, "library": plan.library, "folder": plan.folder }));
    } else {
        println!("Added {} → {} (library {name:?})", plan.describe, path.display());
        if !plan.other_libraries.is_empty() {
            println!(
                "  The site also has: {}. To sync one of those instead, `looper-sharepoint remove {name}` and add again with --library NAME.",
                plan.other_libraries.join(", ")
            );
        }
    }
    let host = reload_host(config_path, data).await;
    if a.no_sync {
        if !as_json {
            println!("{}", next_steps(host));
        }
        return Ok(ExitCode::SUCCESS);
    }
    if !as_json {
        eprintln!("First sync…");
    }
    let code = super::sync(&new_cfg, data, Some(name), RunOptions::default(), as_json).await?;
    if !as_json {
        println!("{}", next_steps(host));
    }
    Ok(code)
}

pub async fn remove(config_path: &Path, data: &Path, name: &str, as_json: bool) -> Result<ExitCode> {
    let old = load(config_path)?;
    let (_, lib) = onboard::remove(config_path, name)?;
    let path = old.local_root(&lib);
    let host = reload_host(config_path, data).await;
    // Not while a host that couldn't reload may still be syncing it.
    let forgot = host != Host::Failed && onboard::forget_state(data, lib.name())?;
    if as_json {
        println!("{}", json!({ "removed": lib.name(), "path": path, "state_forgotten": forgot }));
    } else {
        println!(
            "Stopped syncing {:?}. Nothing was deleted: the files are still in {} and in SharePoint ({}).",
            lib.name(),
            path.display(),
            lib.site
        );
    }
    Ok(ExitCode::SUCCESS)
}

/// `setup <file or link>`: write (or update) the config's `[account]`, add the file's libraries, and offer to sign in.
pub async fn setup(config_path: &Path, data: &Path, source: &str, yes: bool, replace: bool, as_json: bool) -> Result<ExitCode> {
    let plan = onboard::plan_setup(config_path, source)?;
    if let Some(r) = &plan.replaces
        && !replace
    {
        bail!(
            "this computer is already set up for another organization or app (tenant {}, app {}). Add --replace to \
             switch: your libraries stay in the config, and you sign in again",
            r.tenant,
            r.client_id
        );
    }
    if !as_json {
        let a = &plan.account;
        println!("This sets up looper-sharepoint for:");
        println!("  organization (tenant): {}", a.tenant);
        println!("  app:                   {} (client id {})", a.app_name.as_deref().unwrap_or("looper-sharepoint"), a.client_id);
        for l in &plan.libraries {
            let what = match &l.folder {
                Some(f) => format!("{} / {} / {f}", l.site, l.library),
                None => format!("{} / {}", l.site, l.library),
            };
            println!("  adds:                  {what} → {}", l.path.display());
        }
        if let Some(r) = &plan.replaces {
            println!("  replaces:              tenant {}, app {}", r.tenant, r.client_id);
        }
        println!("  config file:           {}", config_path.display());
    }
    if !yes && !super::confirm("Continue?")? {
        println!("Nothing changed.");
        return Ok(ExitCode::from(1));
    }
    let done = plan.apply(config_path, replace)?;
    for s in &done.skipped {
        eprintln!("warning: skipped {s}");
    }
    let cfg = done.config.clone();
    let host = if done.switched { Host::None } else { reload_host(config_path, data).await };
    if as_json {
        println!("{}", json!({ "config": config_path, "tenant": cfg.account.tenant, "app_name": cfg.account.app_name, "added": done.added }));
    } else {
        println!("Set up: {}", config_path.display());
        if done.switched {
            println!("If looper-sharepoint is running, quit it and start it again.");
        }
    }
    looper_sharepoint_engine::account::configure_client(&cfg);
    let signed_in = !done.switched && super::tokens(&cfg, true).is_ok();
    if !signed_in && !as_json {
        if !yes && std::io::IsTerminal::is_terminal(&std::io::stdin()) && confirm_yes("Sign in now?")? {
            super::login(&cfg, cfg.account.auth_flow == AuthFlow::DeviceCode).await?;
            let _ = looper_sharepoint_ipc::request(data, &Request::SignedIn).await;
        } else {
            println!("Next: `looper-sharepoint login` to sign in.");
        }
    }
    if !as_json {
        if cfg.libraries.is_empty() {
            println!(
                "Then add what to sync: open the site or folder in SharePoint, copy the address, and run\n  \
                 looper-sharepoint add \"<link>\" --to ~/<folder>"
            );
        } else if host != Host::Reloaded {
            println!("{}", next_steps(host));
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// `setup export`: a setup file (or link) for others, from this computer's config.
pub fn setup_export(config_path: &Path, with_libraries: bool, output: Option<&Path>, link: bool) -> Result<ExitCode> {
    let cfg = Config::load(config_path).context("loading config (see `looper-sharepoint config path`)")?;
    let default = |s: &str| s == "looper-sharepoint";
    if link {
        let a = &cfg.account;
        let app = (!default(&a.app_name)).then_some(a.app_name.as_str());
        let publisher = (!default(&a.publisher)).then_some(a.publisher.as_str());
        println!("{}", links::setup_link(&a.tenant, &a.client_id, app, publisher));
        return Ok(ExitCode::SUCCESS);
    }
    let text = looper_sharepoint_engine::core::setup::export(&cfg, with_libraries);
    match output {
        Some(p) => {
            std::fs::write(p, &text).with_context(|| format!("writing {}", p.display()))?;
            eprintln!("Wrote {}. Send it with the app; it holds no passwords or secrets.", p.display());
        }
        None => print!("{text}"),
    }
    Ok(ExitCode::SUCCESS)
}

fn load(config_path: &Path) -> Result<Config> {
    if !config_path.exists() {
        bail!(
            "not set up yet: run `looper-sharepoint setup <file or link>` first, with the setup file or \
             looper-sharepoint:// link from whoever gave you this tool (or create {} by hand: see config.example.toml)",
            config_path.display()
        );
    }
    let cfg = Config::load(config_path).context("loading config (see `looper-sharepoint config path`)")?;
    looper_sharepoint_engine::account::configure_client(&cfg);
    Ok(cfg)
}

/// A Graph client for a signed-in account, signing in first on a terminal.
async fn signed_in_graph(cfg: &Config, data: &Path) -> Result<looper_sharepoint_engine::graph::GraphClient> {
    match super::graph(cfg) {
        Ok(g) => match g.me().await {
            Ok(_) => return Ok(g),
            Err(e) if !e.is_auth() => return Err(e.into()),
            Err(_) => {}
        },
        Err(e) if !e.to_string().contains("not signed in") => return Err(e),
        Err(_) => {}
    }
    if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        bail!("not signed in; run `looper-sharepoint login` first");
    }
    eprintln!("Signing in first…");
    super::login(cfg, cfg.account.auth_flow == AuthFlow::DeviceCode).await?;
    let _ = looper_sharepoint_ipc::request(data, &Request::SignedIn).await;
    super::graph(cfg)
}

fn tilde(p: &Path) -> String {
    match dirs::home_dir().and_then(|h| p.strip_prefix(&h).ok().map(Path::to_path_buf)) {
        Some(rest) => format!("~/{}", rest.display()),
        None => p.display().to_string(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Host {
    /// No host is running.
    None,
    Reloaded,
    /// A host is running but didn't take the change (it said why).
    Failed,
}

/// Tell a running host the config changed.
async fn reload_host(config_path: &Path, data: &Path) -> Host {
    let config = std::fs::canonicalize(config_path).unwrap_or_else(|_| config_path.to_owned());
    match looper_sharepoint_ipc::request(data, &Request::Reload { config }).await {
        Ok(None) => Host::None,
        Ok(Some(Response::Ok)) => Host::Reloaded,
        Ok(Some(Response::Error { message })) => {
            eprintln!("warning: the running looper-sharepoint didn't pick up the change: {message}. Restart it.");
            Host::Failed
        }
        Ok(Some(other)) => {
            eprintln!("warning: unexpected answer from the running looper-sharepoint: {other:?}. Restart it.");
            Host::Failed
        }
        Err(e) => {
            eprintln!("warning: couldn't reach the running looper-sharepoint ({e:#}). Restart it.");
            Host::Failed
        }
    }
}

fn next_steps(host: Host) -> &'static str {
    match host {
        Host::Reloaded => "The running looper-sharepoint keeps it in sync from now on.",
        _ => "To keep it in sync from now on, open the Looper SharePoint app (menu bar), or run `looper-sharepoint run`.",
    }
}

/// A yes/no question where Enter means yes.
fn confirm_yes(question: &str) -> Result<bool> {
    eprint!("{question} [Y/n] ");
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(!matches!(line.trim().to_lowercase().as_str(), "n" | "no"))
}

/// `start-at-login on|off|status`. With the app installed, its login item (one switch with the tray's menu); with
/// only the command line tool, a LaunchAgent that runs `looper-sharepoint run --no-tui`. `specs/onboarding-design.md` §8.
#[cfg(unix)]
pub fn start_at_login(config_path: &Path, data: &Path, what: super::LoginWhat, as_json: bool) -> Result<ExitCode> {
    use looper_sharepoint_engine::login_item::{self, AppItem};
    let app = login_item::app_unavailable().is_none();
    let agent = login_item::agent_installed();
    let say = |text: String, state: &str| {
        if as_json {
            println!("{}", json!({ "start_at_login": state, "how": if app { "app" } else { "launch_agent" } }));
        } else {
            println!("{text}");
        }
    };
    match what {
        super::LoginWhat::Status => {
            let state = match (app, login_item::app_status(), agent) {
                (true, AppItem::On, _) => ("on", "On: Looper SharePoint starts at login (System Settings → General → Login Items).".to_string()),
                (true, AppItem::NeedsApproval, _) => ("needs_approval", "Waiting for you to allow it in System Settings → General → Login Items.".to_string()),
                (_, _, true) => ("on", format!("On: `looper-sharepoint run` starts at login ({}).", login_item::agent_plist()?.display())),
                _ => ("off", "Off. `looper-sharepoint start-at-login on` to turn it on.".to_string()),
            };
            say(state.1, state.0);
        }
        super::LoginWhat::On if app => {
            if agent {
                login_item::remove_agent()?;
            }
            let state = login_item::set_app(true)?;
            let _ = login_item::save_prefs(data, &login_item::Prefs { user_turned_off: false, ..login_item::load_prefs(data) });
            match state {
                AppItem::NeedsApproval => {
                    login_item::open_settings();
                    say("Almost: allow Looper SharePoint in System Settings → General → Login Items (opened).".into(), "needs_approval");
                }
                _ => say("On: Looper SharePoint starts at login. It shows in System Settings → General → Login Items.".into(), "on"),
            }
        }
        super::LoginWhat::On => {
            let exe = std::env::current_exe().and_then(std::fs::canonicalize).context("finding this program")?;
            if looper_sharepoint_engine::cli_link::location(&exe) != looper_sharepoint_engine::cli_link::Location::Installed {
                bail!(
                    "{} is a development build or a temporary copy; install looper-sharepoint (or the app) first",
                    exe.display()
                );
            }
            let config = std::fs::canonicalize(config_path).with_context(|| format!("no config at {}; set up first", config_path.display()))?;
            let p = login_item::install_agent(&exe, &config, data)?;
            say(
                format!("On: `looper-sharepoint run` starts at your next login ({}). To start it now: `looper-sharepoint run`.", p.display()),
                "on",
            );
        }
        super::LoginWhat::Off => {
            let mut any = false;
            if app && login_item::app_status() != AppItem::Off {
                login_item::set_app(false)?;
                any = true;
            }
            let _ = login_item::save_prefs(data, &login_item::Prefs { user_turned_off: true, ..login_item::load_prefs(data) });
            any |= login_item::remove_agent()?;
            say(if any { "Off: nothing starts at login.".into() } else { "It was already off.".into() }, "off");
        }
    }
    Ok(ExitCode::SUCCESS)
}
