//! `add` and `remove`: start or stop syncing a SharePoint library or folder, from a link. See
//! `specs/onboarding-design.md` §5.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use looper_sharepoint_engine::core::config::{Config, expand_tilde};
use looper_sharepoint_engine::core::config_edit::{self, NewLibrary};
use looper_sharepoint_engine::links::{self, AppLink, Found};
use looper_sharepoint_engine::{RunOptions, index};
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
    let (link, suggested_to) = match links::parse_app_link(&a.link)? {
        Some(AppLink::Add { url, to }) => (url, to),
        Some(AppLink::Setup { .. }) => bail!("that's a setup link: run `looper-sharepoint setup \"{}\"` first", a.link),
        None => (a.link.clone(), None),
    };
    let text = read_config(config_path)?;
    let cfg = Config::parse(&text, config_path).context("loading config (see `looper-sharepoint config path`)")?;
    looper_sharepoint_engine::account::configure_client(&cfg);
    let graph = signed_in_graph(&cfg).await?;
    let found = links::find(&graph, &link, a.library.as_deref()).await?;

    if let Some(lib) = cfg.libraries.iter().find(|l| same_place(l, &found)) {
        let path = cfg.local_root(lib);
        if as_json {
            println!("{}", json!({ "added": false, "already": lib.name(), "path": path }));
        } else {
            println!("Already syncing {} to {} (library {:?}).", found.describe(), path.display(), lib.name());
        }
        return Ok(ExitCode::SUCCESS);
    }

    let suggestion = match (&a.to, suggested_to) {
        (Some(to), _) => to.clone(),
        (None, Some(to)) => PathBuf::from(to),
        (None, None) => cfg.root().join(found.suggested_name()),
    };
    let to = if a.to.is_none() && !a.yes && !as_json && std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        PathBuf::from(super::prompt("Local folder", &tilde(&suggestion))?)
    } else {
        suggestion
    };
    let to = absolute(&to)?;
    if !a.adopt && has_files(&to) {
        bail!(
            "{} isn't empty. Choose an empty or new folder, or add --adopt to keep what's in it: files that match \
             SharePoint's are kept, and files only in this folder are uploaded{}",
            to.display(),
            if a.read_only { " (with --read-only: kept, not uploaded)" } else { "" }
        );
    }

    let new = NewLibrary {
        site: found.site_url.clone(),
        library: found.library.clone(),
        folder: found.folder.clone(),
        path: to.clone(),
        name: a.name.clone(),
        read_only: a.read_only,
    };
    let (text, new_cfg) = config_edit::with_library(&text, config_path, &new).map_err(|e| anyhow::anyhow!("not added: {e}"))?;
    config_edit::write(config_path, &text).with_context(|| format!("writing {}", config_path.display()))?;
    let lib = new_cfg.libraries.last().expect("just added");
    let name = lib.name().to_string();
    if as_json {
        println!("{}", json!({ "added": true, "name": name, "path": to, "site": found.site_url, "library": found.library, "folder": found.folder }));
    } else {
        println!("Added {} → {} (library {name:?})", found.describe(), to.display());
        if !found.other_libraries.is_empty() {
            println!(
                "  The site also has: {}. To sync one of those instead, `looper-sharepoint remove {name}` and add again with --library NAME.",
                found.other_libraries.join(", ")
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
    let text = read_config(config_path)?;
    let old = Config::parse(&text, config_path)?;
    let (text, lib) = config_edit::without_library(&text, config_path, name).map_err(|e| anyhow::anyhow!("{e}"))?;
    let path = old.local_root(&lib);
    config_edit::write(config_path, &text).with_context(|| format!("writing {}", config_path.display()))?;
    let host = reload_host(config_path, data).await;
    // The library's sync state isn't the user's data: forget it, so adding it again starts fresh. Not while a host
    // that couldn't reload may still be syncing it.
    let forgot = if host != Host::Failed {
        match index::forget(data, lib.name())? {
            Some(e) => {
                for ext in ["", "-wal", "-shm"] {
                    let _ = std::fs::remove_file(format!("{}{ext}", e.db_path.display()));
                }
                true
            }
            None => false,
        }
    } else {
        false
    };
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

fn read_config(config_path: &Path) -> Result<String> {
    if !config_path.exists() {
        bail!(
            "not set up yet: run `looper-sharepoint setup <file or link>` first, with the setup file or \
             looper-sharepoint:// link from whoever gave you this tool (or create {} by hand: see config.example.toml)",
            config_path.display()
        );
    }
    std::fs::read_to_string(config_path).with_context(|| format!("reading {}", config_path.display()))
}

/// A Graph client for a signed-in account, signing in first on a terminal.
async fn signed_in_graph(cfg: &Config) -> Result<looper_sharepoint_engine::graph::GraphClient> {
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
    super::login(cfg, cfg.account.auth_flow == looper_sharepoint_engine::core::config::AuthFlow::DeviceCode).await?;
    let _ = looper_sharepoint_ipc::request(&looper_sharepoint_engine::core::config::data_dir(), &Request::SignedIn).await;
    super::graph(cfg)
}

/// Is `lib` already syncing what `found` points to?
fn same_place(lib: &looper_sharepoint_engine::core::config::Library, found: &Found) -> bool {
    let norm = |s: &str| s.trim_end_matches('/').to_lowercase();
    norm(&lib.site) == norm(&found.site_url)
        && (lib.library.eq_ignore_ascii_case(&found.library) || lib.library == found.drive_id)
        && lib.folder.as_deref().map(str::to_lowercase) == found.folder.as_deref().map(str::to_lowercase)
}

fn has_files(dir: &Path) -> bool {
    std::fs::read_dir(dir).is_ok_and(|mut d| d.any(|e| e.is_ok_and(|e| e.file_name() != ".DS_Store")))
}

fn absolute(p: &Path) -> Result<PathBuf> {
    let p = expand_tilde(&p.to_string_lossy());
    Ok(std::path::absolute(&p)?)
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
        _ => {
            "To keep it in sync from now on, open the Looper SharePoint app (menu bar), or run `looper-sharepoint run`."
        }
    }
}

/// `setup <file or link>`: write (or update) the config's `[account]` from a setup file or `looper-sharepoint://setup`
/// link, add the file's libraries, and offer to sign in. `specs/onboarding-design.md` §4.
pub async fn setup(config_path: &Path, data: &Path, source: &str, yes: bool, replace: bool, as_json: bool) -> Result<ExitCode> {
    use looper_sharepoint_engine::core::setup::SetupFile;
    let (account, libraries) = match links::parse_app_link(source)? {
        Some(AppLink::Setup { tenant, client_id, app_name, publisher }) => {
            (config_edit::AccountValues { tenant, client_id, app_name, publisher, allowed_tenants: vec![] }, vec![])
        }
        Some(AppLink::Add { .. }) => bail!("that's a link to add a library: run `looper-sharepoint add \"{source}\"` (after setup)"),
        None => {
            let p = expand_tilde(source);
            if source.starts_with("http://") || source.starts_with("https://") {
                bail!("{source} is a web address: download the setup file first, then run `looper-sharepoint setup <file>`");
            }
            let text = std::fs::read_to_string(&p).with_context(|| format!("reading the setup file {}", p.display()))?;
            let f = SetupFile::parse(&text, &p).map_err(|e| anyhow::anyhow!("{e}"))?;
            (f.account_values(), f.new_libraries())
        }
    };
    let existing = if config_path.exists() { Some(std::fs::read_to_string(config_path)?) } else { None };
    let current = match &existing {
        Some(t) => Some(Config::parse(t, config_path).with_context(|| {
            format!("{} has a problem; fix it (or move it away) before running setup", config_path.display())
        })?),
        None => None,
    };
    let switching = current.as_ref().filter(|c| {
        !c.account.tenant.eq_ignore_ascii_case(&account.tenant) || !c.account.client_id.eq_ignore_ascii_case(&account.client_id)
    });
    if let Some(c) = switching
        && !replace
    {
        bail!(
            "this computer is already set up for another organization or app (tenant {}, app {}). Add --replace to \
             switch: your libraries stay in the config, and you sign in again",
            c.account.tenant,
            c.account.client_id
        );
    }
    let app = account.app_name.clone().unwrap_or_else(|| "looper-sharepoint".into());
    let to_add: Vec<&NewLibrary> = libraries
        .iter()
        .filter(|l| {
            !current.as_ref().is_some_and(|c| {
                c.libraries.iter().any(|x| {
                    x.site.trim_end_matches('/').eq_ignore_ascii_case(l.site.trim_end_matches('/'))
                        && x.library.eq_ignore_ascii_case(&l.library)
                        && x.folder.as_deref().map(str::to_lowercase) == l.folder.as_deref().map(str::to_lowercase)
                })
            })
        })
        .collect();
    if !as_json {
        println!("This sets up looper-sharepoint for:");
        println!("  organization (tenant): {}", account.tenant);
        println!("  app:                   {app} (client id {})", account.client_id);
        for l in &to_add {
            let what = match &l.folder {
                Some(f) => format!("{} / {} / {f}", l.site, l.library),
                None => format!("{} / {}", l.site, l.library),
            };
            println!("  adds:                  {what} → {}", l.path.display());
        }
        if let Some(c) = switching {
            println!("  replaces:              tenant {}, app {}", c.account.tenant, c.account.client_id);
        }
        println!("  config file:           {}", config_path.display());
    }
    if !yes && !super::confirm("Continue?")? {
        println!("Nothing changed.");
        return Ok(ExitCode::from(1));
    }
    let (mut text, _) = config_edit::with_account(existing.as_deref(), config_path, &account).map_err(|e| anyhow::anyhow!("{e}"))?;
    let mut added = vec![];
    for l in to_add {
        match config_edit::with_library(&text, config_path, l) {
            Ok((t, cfg)) => {
                text = t;
                added.push(cfg.libraries.last().map(|x| x.name().to_string()).unwrap_or_default());
            }
            Err(e) => eprintln!("warning: skipped {} / {}: {e}", l.site, l.library),
        }
    }
    config_edit::write(config_path, &text).with_context(|| format!("writing {}", config_path.display()))?;
    let cfg = Config::parse(&text, config_path)?;
    let host = if switching.is_some() { Host::None } else { reload_host(config_path, data).await };
    if as_json {
        println!("{}", json!({ "config": config_path, "tenant": cfg.account.tenant, "app_name": cfg.account.app_name, "added": added }));
    } else {
        println!("Set up: {}", config_path.display());
        if switching.is_some() {
            println!("If looper-sharepoint is running, quit it and start it again.");
        }
    }
    looper_sharepoint_engine::account::configure_client(&cfg);
    let signed_in = switching.is_none() && super::tokens(&cfg, true).is_ok();
    if !signed_in && !as_json {
        if !yes && std::io::IsTerminal::is_terminal(&std::io::stdin()) && confirm_yes("Sign in now?")? {
            super::login(&cfg, cfg.account.auth_flow == looper_sharepoint_engine::core::config::AuthFlow::DeviceCode).await?;
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

/// A yes/no question where Enter means yes.
fn confirm_yes(question: &str) -> Result<bool> {
    eprint!("{question} [Y/n] ");
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(!matches!(line.trim().to_lowercase().as_str(), "n" | "no"))
}
