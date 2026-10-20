//! `looper-sharepoint`: sync SharePoint document libraries to local folders.
//!
//! This binary is one host for `looper-sharepoint-engine`: one-shot commands, plus `run`, a foreground service with a
//! TUI. Everything sync-related lives in the engine; this file is argument parsing and presentation.

mod fmt;
mod tui;

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use serde_json::json;
use looper_sharepoint_engine::access::{self, Role};
use looper_sharepoint_engine::account::{self, SignInPrompt};
#[cfg(unix)]
use looper_sharepoint_engine::cli_link;
use looper_sharepoint_engine::auth::{PasswordProvider, TokenProvider, TokenStore};
use looper_sharepoint_engine::core::config::{AuthFlow, Config, data_dir};
use looper_sharepoint_engine::core::state::StateDb;
use looper_sharepoint_engine::graph::GraphClient;
use looper_sharepoint_engine::{
    Event, HostLock, LibraryState, LibraryStatus, Outcome, PushState, RunOptions, RunRecord, Service, ServiceConfig, Status,
    Summary,
    Trigger, index, no_progress, resolve, sync_library,
};
use looper_sharepoint_ipc::{Request, Response};

/// Test-only: when both are set, sign in with the password grant instead of the keychain (MFA-less test accounts).
const ENV_TEST_USER: &str = "LOOPER_SHAREPOINT_TEST_USERNAME";
const ENV_TEST_PASSWORD: &str = "LOOPER_SHAREPOINT_TEST_PASSWORD";

#[derive(Parser)]
#[command(name = "looper-sharepoint", version, about = "Sync SharePoint document libraries to local folders")]
struct Cli {
    /// Path to config.toml (default: the OS config dir, e.g. ~/Library/Application Support/looper-sharepoint/config.toml)
    #[arg(long, global = true, env = "LOOPER_SHAREPOINT_CONFIG")]
    config: Option<PathBuf>,
    /// Machine-readable output
    #[arg(long, global = true)]
    json: bool,
    /// More logging (-v info, -vv debug)
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    verbose: u8,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Sign in (opens a browser; --device-code for headless machines)
    Login {
        #[arg(long)]
        device_code: bool,
    },
    /// Forget the stored sign-in
    Logout,
    /// Show the signed-in account
    Whoami,
    /// List sites and document libraries you can access, as ready-to-paste [[library]] blocks
    Libraries {
        /// Only this site (URL)
        #[arg(long)]
        site: Option<String>,
    },
    /// Configuration commands
    Config {
        #[command(subcommand)]
        cmd: ConfigCmd,
    },
    /// Sync now and wait. Uses the running host if there is one; otherwise syncs in this process
    Sync {
        /// Only the library with this `local` name
        #[arg(long)]
        library: Option<String>,
        /// Show what would happen without changing anything
        #[arg(long)]
        dry_run: bool,
        /// Proceed even if more than sync.max_delete_fraction of local files would be deleted
        #[arg(long)]
        allow_mass_delete: bool,
    },
    /// Run the sync service in the foreground (TUI on a terminal; log lines otherwise)
    Run {
        /// Plain log lines instead of the TUI
        #[arg(long)]
        no_tui: bool,
    },
    /// Ask the running host to sync now, without waiting
    Trigger {
        #[arg(long)]
        library: Option<String>,
        /// Reconnect live updates instead (skips any reconnect backoff)
        #[arg(long)]
        reconnect: bool,
    },
    /// Show per-library sync state and recent runs (live from the running host, else from local state)
    Status {
        /// How many recent runs to show
        #[arg(long, default_value_t = 10)]
        history: usize,
    },
    /// Who can read or write a folder or file: show, set, check
    Access {
        #[command(subcommand)]
        cmd: AccessCmd,
    },
    /// Open a folder or file in SharePoint, or one of its pages (permissions, versions, recycle bin…)
    Web {
        #[command(flatten)]
        at: ItemArg,
        /// Which page
        #[arg(value_enum, default_value_t = WebPage::Item)]
        page: WebPage,
        /// Print the link instead of opening it
        #[arg(long)]
        print: bool,
    },
    /// Put this `looper-sharepoint` on the PATH (a link to it, so app updates carry over)
    #[cfg(unix)]
    InstallCli {
        /// Just for you: ~/.local/bin (the default unless /usr/local/bin is writable without a password)
        #[arg(long, conflicts_with = "system")]
        user: bool,
        /// For all users: /usr/local/bin (uses sudo if needed)
        #[arg(long)]
        system: bool,
        /// With --user: add ~/.local/bin to your shell's startup file if it isn't on your PATH
        #[arg(long)]
        add_to_path: bool,
        /// Replace a link to something else (never a regular file)
        #[arg(long)]
        force: bool,
    },
    /// Remove the link `install-cli` made (only if it's still ours)
    #[cfg(unix)]
    UninstallCli {
        /// Don't ask; also remove the PATH line `install-cli` added
        #[arg(long)]
        yes: bool,
    },
}

/// A folder or file: a local path in a synced library folder, or `--library NAME` and a path in that library.
#[derive(clap::Args)]
struct ItemArg {
    /// Local path (or, with --library, the path inside the library; empty for its root)
    #[arg(default_value = ".")]
    path: String,
    /// The library (its `local` name in config); PATH is then a path in SharePoint
    #[arg(long)]
    library: Option<String>,
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum WebPage {
    /// The folder or file
    Item,
    /// Advanced permissions (inheritance, groups, "Check Permissions")
    Permissions,
    /// Version history (files)
    Versions,
    RecycleBin,
    /// The site's groups and permission levels
    SitePermissions,
    LibrarySettings,
    Site,
}

#[derive(Subcommand)]
enum AccessCmd {
    /// List who has access, and whether the item has its own permissions
    Show {
        #[command(flatten)]
        at: ItemArg,
    },
    /// Make exactly these people the ones who can read and write it (the site owners always keep access)
    Set {
        #[command(flatten)]
        at: ItemArg,
        /// People who can read (comma-separated email addresses or user names)
        #[arg(long, value_delimiter = ',')]
        read: Vec<String>,
        /// People who can read and write
        #[arg(long, value_delimiter = ',')]
        write: Vec<String>,
        /// Nobody but the site owners
        #[arg(long, conflicts_with_all = ["read", "write"])]
        owners_only: bool,
        /// Don't ask before changing anything
        #[arg(long)]
        yes: bool,
        /// Show what would change, and change nothing
        #[arg(long)]
        dry_run: bool,
        /// Allow the library root (restricts the whole library)
        #[arg(long)]
        allow_root: bool,
        /// Email the people granted access
        #[arg(long)]
        notify: bool,
        /// Allow people outside the organisation (if SharePoint's sharing settings allow it)
        #[arg(long)]
        allow_external: bool,
    },
    /// Can this person read or write it? PERSON is an email address or user name, or `me` (tested for real)
    Check {
        person: String,
        #[command(flatten)]
        at: ItemArg,
    },
    /// Make it inherit its parent folder's permissions again. Graph can't do this, so it opens SharePoint's page
    Inherit {
        #[command(flatten)]
        at: ItemArg,
        /// Print the link instead of opening it
        #[arg(long)]
        print: bool,
    },
}

#[derive(Subcommand)]
enum ConfigCmd {
    /// Validate config.toml and resolve each library against SharePoint
    Check,
    /// Print the config file path
    Path,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let tui = matches!(cli.cmd, Cmd::Run { no_tui: false }) && std::io::IsTerminal::is_terminal(&std::io::stdout());
    let log_file = data_dir().join("logs").join("looper-sharepoint.log");
    if let Err(e) = init_logging(cli.verbose, matches!(cli.cmd, Cmd::Run { .. }), tui.then_some(log_file.as_path())) {
        eprintln!("error: {e:#}");
        return ExitCode::from(2);
    }
    match run(cli, tui, &log_file).await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::from(2)
        }
    }
}

fn init_logging(verbose: u8, service: bool, file: Option<&Path>) -> Result<()> {
    let level = match (verbose, service) {
        (0, false) => "warn",
        (0, true) | (1, _) => "info",
        _ => "debug",
    };
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(format!("{level},hyper=warn,reqwest=warn")));
    let fmt = tracing_subscriber::fmt().with_env_filter(filter);
    match file {
        // The TUI owns the terminal; logs go to a file.
        Some(path) => {
            std::fs::create_dir_all(path.parent().unwrap())?;
            let f = std::fs::OpenOptions::new().create(true).append(true).open(path).with_context(|| format!("opening {}", path.display()))?;
            fmt.with_ansi(false).with_writer(std::sync::Mutex::new(f)).init();
        }
        None => fmt.with_writer(std::io::stderr).init(),
    }
    Ok(())
}

async fn run(cli: Cli, tui: bool, log_file: &Path) -> Result<ExitCode> {
    let config_path = cli.config.clone().unwrap_or_else(Config::default_path);
    let load = || {
        if !config_path.exists() {
            anyhow::bail!(
                "not set up yet: no config file at {}. Create it with your organization's tenant and client_id \
                 (see config.example.toml and docs/user-guide.md)",
                config_path.display()
            );
        }
        let cfg = Config::load(&config_path).context("loading config (see `looper-sharepoint config path`)")?;
        account::configure_client(&cfg);
        anyhow::Ok(cfg)
    };
    let data = data_dir();
    match cli.cmd {
        #[cfg(unix)]
        Cmd::InstallCli { user, system, add_to_path, force } => {
            let scope = if user { Some(cli_link::Scope::User) } else if system { Some(cli_link::Scope::System) } else { None };
            return install_cli(&data, scope, add_to_path, force);
        }
        #[cfg(unix)]
        Cmd::UninstallCli { yes } => return uninstall_cli(&data, yes),
        Cmd::Config { cmd: ConfigCmd::Path } => {
            println!("{}", config_path.display());
        }
        Cmd::Login { device_code } => {
            let cfg = load()?;
            login(&cfg, device_code || cfg.account.auth_flow == AuthFlow::DeviceCode).await?;
            // A running host may be waiting for this.
            let _ = looper_sharepoint_ipc::request(&data, &Request::SignedIn).await;
        }
        Cmd::Logout => match account::sign_out(&load()?, &data)? {
            Some(a) => println!("Signed out {}", a.username),
            None => println!("Not signed in"),
        },
        Cmd::Whoami => {
            let cfg = load()?;
            let graph = graph(&cfg)?;
            let me = graph.me().await?;
            if cli.json {
                println!(
                    "{}",
                    json!({ "user": me.user_principal_name, "name": me.display_name, "tenant": cfg.account.tenant,
                            "app_name": cfg.account.app_name, "client_id": cfg.account.client_id })
                );
            } else {
                println!(
                    "{} ({}) in tenant {}",
                    me.user_principal_name.unwrap_or_default(),
                    me.display_name.unwrap_or_default(),
                    cfg.account.tenant
                );
                println!("app: {} (client id {})", cfg.account.app_name, cfg.account.client_id);
            }
        }
        Cmd::Libraries { site } => {
            let cfg = load()?;
            libraries(&graph(&cfg)?, site.as_deref(), cli.json).await?;
        }
        Cmd::Config { cmd: ConfigCmd::Check } => {
            let cfg = load()?;
            println!("{}: OK ({} libraries)", config_path.display(), cfg.libraries.len());
            let graph = graph(&cfg)?;
            let mut ok = true;
            for lib in &cfg.libraries {
                match resolve(&graph, lib).await {
                    Ok(r) => println!("  {:<20} {} / {} → drive {}", lib.local, lib.site, lib.library, r.drive_id),
                    Err(e) => {
                        ok = false;
                        println!("  {:<20} ERROR: {e:#}", lib.local);
                    }
                }
            }
            if !ok {
                return Ok(ExitCode::from(1));
            }
        }
        Cmd::Sync { library, dry_run, allow_mass_delete } => {
            let cfg = load()?;
            if cfg.libraries.is_empty() {
                bail!("no [[library]] entries in {}; `looper-sharepoint libraries` lists what you can sync", config_path.display());
            }
            if let Some(l) = &library {
                cfg.library(l).with_context(|| format!("no library with local = {l:?} in config"))?;
            }
            let opts = RunOptions { dry_run, allow_mass_delete };
            return sync(&cfg, &data, library, opts, cli.json).await;
        }
        Cmd::Run { .. } => {
            let cfg = load()?;
            return host(cfg, data, tui, log_file).await;
        }
        Cmd::Trigger { library, reconnect } => {
            let req = if reconnect { Request::Reconnect { library } } else { Request::Trigger { library } };
            match looper_sharepoint_ipc::request(&data, &req).await? {
            Some(Response::Ok) if reconnect => println!("Reconnecting live updates."),
            Some(Response::Ok) => println!("Sync requested."),
            Some(Response::Error { message }) => bail!(message),
            Some(other) => bail!("unexpected answer from host: {other:?}"),
            None => bail!("no looper-sharepoint host is running (start one with `looper-sharepoint run`)"),
            }
        }
        Cmd::Status { history } => {
            let cfg = load()?;
            let (status, runs, live) = match looper_sharepoint_ipc::request(&data, &Request::Status { history }).await? {
                Some(Response::Status { status, history }) => (status, history, true),
                Some(Response::Error { message }) => bail!(message),
                Some(other) => bail!("unexpected answer from host: {other:?}"),
                None => {
                    let (s, h) = offline_status(&cfg, &data, history);
                    (s, h, false)
                }
            };
            if cli.json {
                println!("{}", serde_json::to_string_pretty(&json!({ "live": live, "status": status, "history": runs }))?);
            } else {
                fmt::print_status(&status, &runs, live);
            }
        }
        Cmd::Access { cmd } => {
            let cfg = load()?;
            return access_cmd(&cfg, &data, cmd, cli.json).await;
        }
        Cmd::Web { at, page, print } => {
            let cfg = load()?;
            let graph = graph(&cfg)?;
            let found = locate(&graph, &cfg, &data, &at).await?;
            let links = access::Links::for_item(&found.item);
            let (url, what) = match page {
                WebPage::Item => (links.item, "the item"),
                WebPage::Permissions => (links.permissions, "advanced permissions"),
                WebPage::Versions => (links.versions, "version history (files only)"),
                WebPage::RecycleBin => (links.recycle_bin, "the recycle bin"),
                WebPage::SitePermissions => (links.site_permissions, "site permissions"),
                WebPage::LibrarySettings => (links.library_settings, "library settings"),
                WebPage::Site => (links.site, "the site"),
            };
            let url = url.with_context(|| format!("SharePoint didn't give a link for {what} here"))?;
            show_link(&url, print, cli.json);
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// `sync`: through the running host if there is one (so the two never fight over the state DB), else in-process.
async fn sync(cfg: &Config, data: &Path, library: Option<String>, opts: RunOptions, as_json: bool) -> Result<ExitCode> {
    let started = std::time::Instant::now();
    let req = Request::Sync { library: library.clone(), options: opts.clone() };
    let results = match looper_sharepoint_ipc::request(data, &req).await? {
        Some(Response::Sync { results }) => {
            if !as_json {
                eprintln!("(synced by the running host)");
            }
            results.into_iter().map(|r| (r.record, r.report)).collect()
        }
        Some(Response::Error { message }) => bail!(message),
        Some(other) => bail!("unexpected answer from host: {other:?}"),
        None => {
            let _lock = HostLock::acquire(data)?;
            let graph = graph(cfg)?;
            let libs: Vec<_> = match &library {
                Some(l) => cfg.library(l).into_iter().collect(),
                None => cfg.libraries.iter().collect(),
            };
            let mut out = vec![];
            for lib in libs {
                let t0 = chrono::Utc::now();
                let r = sync_library(&graph, cfg, data, lib, &opts, &no_progress).await;
                out.push(one_shot_record(lib.local.clone(), t0, r));
            }
            if !as_json {
                let stats = graph.stats();
                eprintln!("Graph requests: {} (throttled: {}, retries: {})", stats.requests, stats.throttled, stats.retries);
            }
            out
        }
    };
    let mut problems = false;
    let mut reports = vec![];
    for (record, report) in &results {
        problems |= record.outcome != Outcome::Ok;
        match report {
            Some(r) => {
                if !as_json {
                    fmt::print_report(r, std::time::Duration::from_millis(record.duration_ms().max(0) as u64));
                }
                reports.push(serde_json::to_value(r)?);
            }
            None => {
                let err = record.error.clone().unwrap_or_default();
                if !as_json {
                    eprintln!("{}: {err}", record.library);
                }
                reports.push(json!({ "library": record.library, "error": err, "outcome": record.outcome }));
            }
        }
    }
    if as_json {
        println!("{}", json!({ "libraries": reports, "took_ms": started.elapsed().as_millis() as u64 }));
    }
    Ok(if problems { ExitCode::from(1) } else { ExitCode::SUCCESS })
}

fn one_shot_record(
    library: String,
    started_at: chrono::DateTime<chrono::Utc>,
    r: Result<looper_sharepoint_engine::Report>,
) -> (RunRecord, Option<looper_sharepoint_engine::Report>) {
    let (outcome, error, report) = match r {
        Ok(r) => (if r.has_problems() { Outcome::Problems } else { Outcome::Ok }, None, Some(r)),
        Err(e) if e.downcast_ref::<looper_sharepoint_engine::MassDeleteRefused>().is_some() => (Outcome::Refused, Some(e.to_string()), None),
        Err(e) => (Outcome::Failed, Some(format!("{e:#}")), None),
    };
    let record = RunRecord {
        library,
        trigger: Trigger::Manual,
        started_at,
        finished_at: chrono::Utc::now(),
        outcome,
        error,
        summary: report.as_ref().map(Summary::from_report),
        issues: vec![],
    };
    (record, report)
}

/// `run`: host the service in the foreground.
async fn host(cfg: Config, data: PathBuf, tui: bool, log_file: &Path) -> Result<ExitCode> {
    if cfg.libraries.is_empty() {
        bail!("no [[library]] entries in config; `looper-sharepoint libraries` lists what you can sync");
    }
    let tokens = tokens(&cfg, false)?;
    let svc = Service::start(ServiceConfig { config: cfg, tokens, data_dir: data })?;
    let _ipc = looper_sharepoint_ipc::serve(svc.clone())?;
    if tui {
        tui::run(svc.clone(), log_file).await?;
    } else {
        log_events(&svc).await;
    }
    eprintln!("Stopping (waiting for any run in progress; Ctrl-C again to force)…");
    tokio::select! {
        _ = svc.shutdown() => {}
        _ = tokio::signal::ctrl_c() => eprintln!("forced stop"),
    }
    Ok(ExitCode::SUCCESS)
}

/// `run --no-tui`: one line per notable event until Ctrl-C.
async fn log_events(svc: &Service) {
    let mut events = svc.subscribe();
    let n = svc.config().libraries.len();
    println!("looper-sharepoint running for {n} {}; Ctrl-C to stop", if n == 1 { "library" } else { "libraries" });
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => return,
            ev = events.recv() => match ev {
                Ok(Event::RunFinished { record }) => println!("{}", fmt::run_line(&record)),
                Ok(Event::Throttled { until }) => println!("{}  throttled by SharePoint until {}", fmt::local_time(chrono::Utc::now()), fmt::local_time(until)),
                Ok(_) => {}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(_) => return,
            },
        }
    }
}

/// Status without a host: what the state DBs remember.
fn offline_status(cfg: &Config, data: &Path, history: usize) -> (Status, Vec<RunRecord>) {
    let idx = index::load(data).unwrap_or_default();
    let mut runs = vec![];
    let mut libs = vec![];
    for lib in &cfg.libraries {
        let mut st = LibraryStatus {
            name: lib.local.clone(),
            local_path: cfg.root().join(&lib.local).display().to_string(),
            site: lib.site.clone(),
            library: lib.library.clone(),
            state: LibraryState::Idle,
            last_success_at: None,
            last_failure: None,
            next_run_at: None,
            consecutive_failures: 0,
            last_run: None,
            push: if cfg.sync.push { PushState::Down { since: chrono::Utc::now(), reason: "no host running".into(), retry_at: None } } else { PushState::Off },
            last_push_at: None,
            watching: false,
            uploads: cfg.uploads(lib),
        };
        if let Some(db) = idx.get(&lib.local.to_lowercase()).and_then(|e| StateDb::open(&e.db_path).ok()) {
            let parse = |j: String| serde_json::from_str::<RunRecord>(&j).ok();
            let recent: Vec<RunRecord> = db.recent_runs(history.max(1)).unwrap_or_default().into_iter().filter_map(parse).collect();
            st.last_run = recent.first().cloned();
            st.last_success_at = db.last_run_with(&["ok", "problems"]).ok().flatten().and_then(parse).map(|r| r.finished_at).or_else(|| {
                let t = db.get_meta(looper_sharepoint_engine::core::state::meta::LAST_SYNC_AT).ok().flatten()?;
                chrono::DateTime::parse_from_rfc3339(&t).ok().map(|t| t.with_timezone(&chrono::Utc))
            });
            st.last_failure = db
                .last_run_with(&["failed", "refused"])
                .ok()
                .flatten()
                .and_then(parse)
                .map(|r| looper_sharepoint_engine::Failure { at: r.finished_at, error: r.error.unwrap_or_default() });
            runs.extend(recent);
        }
        libs.push(st);
    }
    runs.sort_by_key(|r| std::cmp::Reverse(r.started_at));
    runs.truncate(history);
    let account = TokenStore::new(data.to_owned()).account(&cfg.account.tenant, &cfg.account.client_id).ok().flatten().map(|a| a.username);
    let status = Status {
        account,
        tenant: cfg.account.tenant.clone(),
        started_at: chrono::Utc::now(),
        paused: false,
        throttled_until: None,
        requests: 0,
        throttled_responses: 0,
        ru_estimate: 0,
        requests_by_kind: Default::default(),
        poll_interval_secs: cfg.sync.poll_interval.as_secs(),
        push_safety_interval_secs: cfg.sync.push_safety_interval.as_secs(),
        libraries: libs,
    };
    (status, runs)
}

/// `require_account`: fail now if nobody is signed in (one-shot commands). A host starts anyway and reports
/// "sign-in required" until someone signs in.
fn tokens(cfg: &Config, require_account: bool) -> Result<Arc<dyn TokenProvider>> {
    if let (Ok(user), Ok(pass)) = (std::env::var(ENV_TEST_USER), std::env::var(ENV_TEST_PASSWORD)) {
        tracing::warn!(user, "using test password sign-in from {ENV_TEST_USER}/{ENV_TEST_PASSWORD}");
        return Ok(PasswordProvider::new(account::authority(cfg), user, pass));
    }
    let provider = account::token_provider(cfg, &data_dir());
    if require_account && provider.account()?.is_none() {
        bail!("not signed in; run `looper-sharepoint login`");
    }
    Ok(provider)
}

fn graph(cfg: &Config) -> Result<GraphClient> {
    Ok(GraphClient::with_rate(tokens(cfg, true)?, cfg.sync.max_requests_per_second))
}

async fn login(cfg: &Config, device_code: bool) -> Result<()> {
    let flow = device_code.then_some(AuthFlow::DeviceCode);
    let acct = account::sign_in(cfg, &data_dir(), flow, |p| match p {
        SignInPrompt::OpenUrl(url) => {
            eprintln!("Opening your browser to sign in. If it doesn't open, visit:\n\n  {url}\n");
            open_browser(url);
        }
        SignInPrompt::DeviceCode(info) => eprintln!("{}", info.message),
    })
    .await?;
    println!("Signed in to {} as {}", cfg.account.app_name, acct.username);
    Ok(())
}

/// Print a link and open it in the browser (unless `print_only`).
fn show_link(url: &str, print_only: bool, as_json: bool) {
    if as_json {
        println!("{}", json!({ "url": url }));
    } else {
        println!("{url}");
    }
    if !print_only {
        open_browser(url);
    }
}

async fn locate(graph: &GraphClient, cfg: &Config, data: &Path, at: &ItemArg) -> Result<access::Located> {
    let target = match &at.library {
        Some(l) => access::Target::InLibrary { library: l, path: if at.path == "." { "" } else { &at.path } },
        None => access::Target::Local(Path::new(&at.path)),
    };
    access::locate(graph, cfg, data, target).await
}

fn item_name(found: &access::Located) -> String {
    if found.path.is_empty() { format!("{} (library root)", found.library.local) } else { format!("{}/{}", found.library.local, found.path) }
}

async fn access_cmd(cfg: &Config, data: &Path, cmd: AccessCmd, as_json: bool) -> Result<ExitCode> {
    let graph = graph(cfg)?;
    match cmd {
        AccessCmd::Show { at } => {
            let found = locate(&graph, cfg, data, &at).await?;
            let grants = access::list(&graph, &found.drive_id, &found.item.id).await?;
            let own = access::has_own_permissions(&graph, &found.drive_id, &found.item, &grants).await?;
            let links = access::Links::for_item(&found.item);
            if as_json {
                println!("{}", serde_json::to_string_pretty(&json!({
                    "item": item_name(&found), "own_permissions": own, "grants": grants, "links": links,
                }))?);
                return Ok(ExitCode::SUCCESS);
            }
            println!("{}", item_name(&found));
            println!(
                "  {}",
                match own {
                    Some(true) => "Has its own permissions (differs from its parent folder)",
                    Some(false) => "Inherits its parent folder's permissions (same list)",
                    None => "Library root: these are the library's permissions",
                }
            );
            for g in &grants {
                let role = g.role.map(Role::as_str).map(str::to_string).unwrap_or_else(|| g.roles.join(","));
                let members = match &g.who {
                    access::Who::SiteGroup { id: Some(id), .. } => access::Links::site_group(&found.item, id).map(|u| format!("\n          members: {u}")),
                    _ => None,
                };
                println!("  {role:<6}  {}{}", g.who.describe(), members.unwrap_or_default());
            }
            if let Some(u) = links.permissions {
                println!("\nIn SharePoint: {u}");
            }
        }
        AccessCmd::Set { at, read, write, owners_only, yes, dry_run, allow_root, notify, allow_external } => {
            if read.is_empty() && write.is_empty() && !owners_only {
                bail!("say who: --read and/or --write (comma-separated), or --owners-only");
            }
            let want = access::Desired::new(&read, &write)?;
            let found = locate(&graph, cfg, data, &at).await?;
            if found.item.is_root() && !allow_root {
                bail!("that's the root of library {:?}: this would restrict the whole library. Add --allow-root if you mean it", found.library.local);
            }
            let outside = want.outside(&graph.verified_domains().await?);
            if !outside.is_empty() && !allow_external {
                bail!("not in your organisation: {}. Check the spelling, or add --allow-external", outside.join(", "));
            }
            let current = access::list(&graph, &found.drive_id, &found.item.id).await?;
            let plan = access::plan(&current, &want);
            if as_json && dry_run {
                println!("{}", serde_json::to_string_pretty(&json!({ "item": item_name(&found), "plan": plan }))?);
                return Ok(ExitCode::SUCCESS);
            }
            if !as_json {
                println!("{}", item_name(&found));
                print_plan(&plan);
            }
            if plan.is_empty() {
                if !as_json {
                    println!("Already set; nothing to do.");
                }
                return Ok(ExitCode::SUCCESS);
            }
            if dry_run {
                return Ok(ExitCode::SUCCESS);
            }
            if !yes && !confirm("Apply these changes?")? {
                println!("Nothing changed.");
                return Ok(ExitCode::from(1));
            }
            let me = graph.me().await?.user_principal_name.context("your account has no user name")?;
            match access::apply(&graph, &found.drive_id, &found.item.id, &want, notify, &me).await {
                Ok(done) => {
                    if as_json {
                        println!("{}", serde_json::to_string_pretty(&json!({ "item": item_name(&found), "applied": done }))?);
                    } else {
                        println!("Done. Now:");
                        for g in &done.after {
                            println!("  {:<6}  {}", g.role.map(Role::as_str).unwrap_or("?"), g.who.describe());
                        }
                    }
                }
                Err(e) => {
                    let Some(stopped) = e.downcast_ref::<access::Stopped>() else { return Err(e) };
                    let d = &stopped.done;
                    if as_json {
                        println!("{}", serde_json::to_string_pretty(&json!({ "item": item_name(&found), "error": stopped.message, "applied": d }))?);
                    } else {
                        for (what, list) in [("granted", &d.granted), ("changed", &d.changed), ("removed", &d.removed)] {
                            if !list.is_empty() {
                                eprintln!("  {what}: {}", list.join(", "));
                            }
                        }
                        eprintln!("error: {}", stopped.message);
                    }
                    return Ok(ExitCode::from(1));
                }
            }
        }
        AccessCmd::Check { person, at } => {
            let me = person.eq_ignore_ascii_case("me");
            let found = match locate(&graph, cfg, data, &at).await {
                Ok(f) => f,
                // SharePoint hides what you can't read, so for you "not found" is the answer.
                Err(e) if me && matches!(e.downcast_ref(), Some(looper_sharepoint_engine::graph::GraphError::NotFound(_))) => {
                    if as_json {
                        println!("{}", json!({ "item": at.path, "person": "me", "access": null, "certain": false }));
                    } else {
                        println!("you: no access, or it doesn't exist (SharePoint shows both as \"not found\")");
                    }
                    return Ok(ExitCode::from(1));
                }
                Err(e) => return Err(e),
            };
            let links = access::Links::for_item(&found.item);
            if me {
                let role = access::check_me(&graph, &found.drive_id, &found.item).await?;
                if as_json {
                    println!("{}", json!({ "item": item_name(&found), "person": "me", "access": role, "certain": true }));
                } else {
                    println!("you: {} (tested)", if role == Role::Write { "read and write" } else { "read only" });
                }
                return Ok(ExitCode::SUCCESS);
            }
            let grants = access::list(&graph, &found.drive_id, &found.item.id).await?;
            let v = access::check(&grants, &person);
            if as_json {
                println!("{}", serde_json::to_string_pretty(&json!({
                    "item": item_name(&found), "person": person, "access": v.direct, "via_groups": v.via_groups,
                    "certain": v.direct.is_some() || v.via_groups.is_empty(), "check_in_sharepoint": links.permissions,
                }))?);
            } else {
                match v.direct {
                    Some(r) => println!("{person}: {} (granted to them directly)", if r == Role::Read { "read only" } else { "read and write" }),
                    None if v.via_groups.is_empty() => println!("{person}: no access (only site administrators can still open it)"),
                    None => println!("{person}: no direct access"),
                }
                if !v.via_groups.is_empty() {
                    let groups: Vec<String> = v.via_groups.iter().map(|(g, r)| format!("{g} ({})", r.as_str())).collect();
                    println!("  {} through these groups, if they're in them: {}", if v.direct.is_some() { "Possibly more" } else { "Possibly" }, groups.join(", "));
                    println!("  Group membership isn't readable with this app's permissions. To be sure, open the page below and use \"Check Permissions\":");
                    if let Some(u) = &links.permissions {
                        println!("  {u}");
                    }
                }
            }
            return Ok(if v.direct.is_some() { ExitCode::SUCCESS } else { ExitCode::from(1) });
        }
        AccessCmd::Inherit { at, print } => {
            let found = locate(&graph, cfg, data, &at).await?;
            let url = access::Links::for_item(&found.item).permissions.context("SharePoint didn't give a permissions page for this item")?;
            if !as_json {
                eprintln!(
                    "Microsoft Graph can't restore inheritance, so this opens SharePoint's permissions page for {}.\n\
                     There, choose \"Delete unique permissions\" (shown only when it has its own).",
                    item_name(&found)
                );
            }
            show_link(&url, print, as_json);
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn print_plan(plan: &access::Plan) {
    for (p, r) in &plan.add {
        println!("  grant   {:<6}  {p}", r.as_str());
    }
    for (g, r) in &plan.change {
        println!("  change  {:<6}  {} (was {})", r.as_str(), g.who.describe(), g.role.map(Role::as_str).unwrap_or("?"));
    }
    for g in &plan.remove {
        println!("  remove  {:<6}  {}", g.role.map(Role::as_str).unwrap_or("?"), g.who.describe());
    }
    for g in &plan.keep {
        let why = if g.is_site_owners() { " (site owners always keep access)" } else { "" };
        println!("  keep    {:<6}  {}{why}", g.role.map(Role::as_str).unwrap_or("?"), g.who.describe());
    }
}

/// `install-cli`: see `specs/install-cli-design.md` §4.2.
#[cfg(unix)]
fn install_cli(data: &Path, scope: Option<cli_link::Scope>, add_to_path: bool, force: bool) -> Result<ExitCode> {
    use cli_link::{Action, Scope};
    let cli = std::env::current_exe().and_then(std::fs::canonicalize).context("finding this program")?;
    let loc = cli_link::check_cli(&cli).map_err(|e| anyhow::anyhow!("can't install the command: {e}"))?;
    let home = cli_link::home()?;
    let scope = scope.unwrap_or_else(cli_link::default_scope);
    let link = scope.link(&home);
    let record = cli_link::load_record(data);
    let existing = cli_link::inspect(&link, &cli, record.as_ref());
    match cli_link::decide(&existing, &link, force) {
        Action::Stop(why) => bail!(why),
        Action::AlreadyInstalled => println!("Already installed: {} → {}", link.display(), cli.display()),
        Action::Create | Action::Replace => {
            if scope == Scope::System && !cli_link::writable(link.parent().unwrap_or(&link)) {
                eprintln!("Linking {} needs administrator rights; sudo will ask for your password.", link.display());
                cli_link::run_with_sudo(&cli_link::privileged_link_commands(&cli, &link))?;
            } else {
                cli_link::link_unprivileged(&cli, &link)?;
            }
            println!("Installed: {} → {}", link.display(), cli.display());
            if loc == cli_link::Location::Dev {
                println!("(a development build)");
            }
        }
    }
    let mut path_edit = record.as_ref().filter(|r| r.link == link).and_then(|r| r.path_edit.clone());
    let shell = cli_link::user_shell();
    let path = cli_link::login_shell_path(&shell);
    if scope == Scope::User && !path.as_deref().is_some_and(|p| cli_link::path_has(p, &scope.dir(&home))) {
        match (cli_link::path_fix(&shell, &home), add_to_path) {
            (cli_link::PathFix::File(f), true) => {
                if cli_link::add_path_block(&f)? {
                    println!("Added ~/.local/bin to your PATH in {}", f.display());
                    path_edit = Some(f);
                }
            }
            (cli_link::PathFix::Fish, true) => {
                cli_link::fish_add_path()?;
                println!("Added ~/.local/bin to your PATH (fish_add_path)");
            }
            (cli_link::PathFix::File(f), false) => {
                println!("~/.local/bin isn't on your PATH. Add this line to {} (or run again with --add-to-path):", f.display());
                println!("  {}", cli_link::PATH_LINE);
            }
            (cli_link::PathFix::Fish, false) => println!("~/.local/bin isn't on your PATH. Run: fish_add_path -U ~/.local/bin (or run again with --add-to-path)"),
            (cli_link::PathFix::Manual, _) => println!("~/.local/bin isn't on your PATH. Add it in your shell's startup file:\n  {}", cli_link::PATH_LINE),
        }
    }
    if let Some(other) = path.as_deref().and_then(|p| cli_link::shadowed_by(p, &link)) {
        println!("Note: your shell will run {} instead, because it comes first on your PATH", other.display());
    }
    cli_link::save_record(data, &cli_link::Record { link, target: cli, scope, installed_at: cli_link::now(), path_edit })?;
    println!("Open a new terminal window and try: looper-sharepoint status");
    Ok(ExitCode::SUCCESS)
}

/// `uninstall-cli`: removes only a link this feature made, and offers to remove the PATH line it added.
#[cfg(unix)]
fn uninstall_cli(data: &Path, yes: bool) -> Result<ExitCode> {
    let Some(rec) = cli_link::load_record(data) else {
        println!("No command line link was installed by looper-sharepoint; nothing to remove.");
        return Ok(ExitCode::SUCCESS);
    };
    if cli_link::still_ours(&rec) {
        if cli_link::writable(rec.link.parent().unwrap_or(&rec.link)) {
            std::fs::remove_file(&rec.link).with_context(|| format!("removing {}", rec.link.display()))?;
        } else {
            eprintln!("Removing {} needs administrator rights; sudo will ask for your password.", rec.link.display());
            cli_link::run_with_sudo(&cli_link::privileged_remove_commands(&rec.link))?;
        }
        println!("Removed {}", rec.link.display());
    } else {
        println!("{} is no longer our link; left as it is.", rec.link.display());
    }
    if let Some(f) = &rec.path_edit {
        let remove = yes || (std::io::IsTerminal::is_terminal(&std::io::stdin()) && confirm(&format!("Also remove the PATH line added to {}?", f.display()))?);
        if remove && cli_link::remove_path_block(f)? {
            println!("Removed the PATH line from {}", f.display());
        } else if !remove {
            println!("Left the PATH line in {} (between the looper-sharepoint markers)", f.display());
        }
    }
    cli_link::clear_record(data)?;
    Ok(ExitCode::SUCCESS)
}

/// Ask a yes/no question on the terminal. Not on a terminal: refuse, so scripts must say --yes.
fn confirm(question: &str) -> Result<bool> {
    if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
        bail!("not on a terminal; add --yes to apply");
    }
    eprint!("{question} [y/N] ");
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(matches!(line.trim().to_lowercase().as_str(), "y" | "yes"))
}

fn open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let r = std::process::Command::new("open").arg(url).status();
    #[cfg(windows)]
    let r = std::process::Command::new("rundll32").args(["url.dll,FileProtocolHandler", url]).status();
    #[cfg(not(any(target_os = "macos", windows)))]
    let r = std::process::Command::new("xdg-open").arg(url).status();
    if let Err(e) = r {
        tracing::warn!("could not open browser: {e}");
    }
}

async fn libraries(graph: &GraphClient, site: Option<&str>, as_json: bool) -> Result<()> {
    let sites = match site {
        Some(url) => vec![graph.site_by_url(url).await?],
        None => {
            let mut all = graph.followed_sites().await.unwrap_or_default();
            for s in graph.search_sites("*").await? {
                if !all.iter().any(|a| a.id == s.id) {
                    all.push(s);
                }
            }
            all
        }
    };
    let mut out = vec![];
    for s in sites {
        let drives = match graph.site_drives(&s.id).await {
            Ok(d) => d,
            Err(e) => {
                tracing::info!(site = s.web_url, "skipping site: {e}");
                continue;
            }
        };
        for d in drives.into_iter().filter(|d| d.drive_type.as_deref() == Some("documentLibrary")) {
            let web = s.web_url.clone().unwrap_or_default();
            let name = d.name.clone().unwrap_or_default();
            if as_json {
                out.push(json!({ "site": web, "site_name": s.display_name, "library": name, "drive_id": d.id }));
            } else {
                let local = format!(
                    "{}-{}",
                    s.name.clone().unwrap_or_else(|| "site".into()),
                    name.to_lowercase().replace(' ', "-")
                )
                .to_lowercase();
                println!(
                    "# {} — {}\n[[library]]\nsite    = {:?}\nlibrary = {:?}\nlocal   = {:?}\n",
                    s.display_name.clone().unwrap_or_default(),
                    d.web_url.unwrap_or_default(),
                    web,
                    name,
                    local
                );
            }
        }
    }
    if as_json {
        println!("{}", serde_json::to_string_pretty(&out)?);
    }
    Ok(())
}
