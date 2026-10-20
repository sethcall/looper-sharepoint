//! `looper-sharepoint-tray`: a menu-bar (macOS) / notification-area (Windows) host for the sync service.
//!
//! Like `looper-sharepoint run`, this is only a host: it embeds `looper-sharepoint-engine` through its public API,
//! renders `Status`, and forwards user actions. No windows exist until the user opens one from the tray (or opens a
//! setup file or `looper-sharepoint://` link). See `specs/m2-design.md` §7 and `specs/onboarding-design.md`.

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use std::path::{Path, PathBuf};
use std::sync::{Mutex, RwLock};
use std::time::Duration;

use chrono::Utc;
use serde::Serialize;
use looper_sharepoint_engine::account::{self, SignInPrompt};
use looper_sharepoint_engine::core::config::{AuthFlow, Config, data_dir};
use looper_sharepoint_engine::graph::GraphClient;
use looper_sharepoint_engine::links::{self, AppLink};
use looper_sharepoint_engine::onboard::{self, AddOptions, AddPlan, SetupDone, SetupPlan};
use looper_sharepoint_engine::{Event, Health, LibraryState, Mode, Outcome, PushState, RunOptions, RunRecord, Service, ServiceConfig, Status, Trigger};
use tauri::image::Image;
#[cfg(target_os = "macos")]
use tauri::menu::CheckMenuItemBuilder;
use tauri::menu::{Menu, MenuBuilder, MenuItemBuilder};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Emitter, Manager, RunEvent, State, WebviewUrl, WebviewWindowBuilder};
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_opener::OpenerExt;

const TRAY_ID: &str = "main";
const STATUS_WINDOW: &str = "status";
/// Menus show relative times; refresh them at least this often.
const MENU_REFRESH: Duration = Duration::from_secs(30);
/// A saved config file is read again once it's been quiet this long.
const CONFIG_QUIET: Duration = Duration::from_millis(800);

struct AppState {
    config_path: PathBuf,
    data_dir: PathBuf,
    rt: RwLock<Running>,
    signing_in: Mutex<bool>,
    /// A setup or add link, or a setup file, opened from outside: for the status window to show.
    pending: Mutex<Option<Pending>>,
    /// The config file was changed and couldn't be loaded: the service keeps its previous config.
    config_problem: Mutex<Option<String>>,
    /// A one-line notice for the status window (e.g. "now starts at login").
    notice: Mutex<Option<String>>,
    /// The last `add_preview`, so `add_apply` doesn't look the link up again.
    last_add: Mutex<Option<(String, AddPlan)>>,
}

/// What runs for the current config. Replaced as a whole when the config file appears or switches organization.
#[derive(Default)]
struct Running {
    config: Option<Config>,
    svc: Option<Service>,
    /// Why there is no service (not set up, bad config, another host running, ...).
    error: Option<String>,
    _ipc: Option<looper_sharepoint_ipc::Server>,
    /// Bumped on every restart, so the old event loop stops.
    generation: u64,
}

impl AppState {
    fn svc(&self) -> Option<Service> {
        self.rt.read().unwrap().svc.clone()
    }

    /// The config in use: the service's (it follows reloads), else the one loaded without a service.
    fn config(&self) -> Option<Config> {
        let rt = self.rt.read().unwrap();
        rt.svc.as_ref().map(Service::config).or_else(|| rt.config.clone())
    }

    fn error(&self) -> Option<String> {
        self.rt.read().unwrap().error.clone()
    }

    fn generation(&self) -> u64 {
        self.rt.read().unwrap().generation
    }
}

/// Opened from outside, waiting for the status window.
#[derive(Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Pending {
    /// A setup file's path, or a `looper-sharepoint://setup` link.
    Setup { source: String },
    /// A SharePoint link (or a `looper-sharepoint://add` link) to add.
    Add { link: String },
}

/// What the status window renders.
#[derive(Serialize)]
struct Snapshot {
    status: Option<Status>,
    history: Vec<RunRecord>,
    error: Option<String>,
    config_path: String,
    headline: String,
    /// Live or polling, for the status window's mode pill.
    mode: Option<Mode>,
    mode_text: Option<String>,
    /// There's a config with an organization in it.
    set_up: bool,
    config_problem: Option<String>,
    notice: Option<String>,
}

fn main() {
    let data = data_dir();
    init_logging(&data);
    let config_path = std::env::var_os("LOOPER_SHAREPOINT_CONFIG").map(PathBuf::from).unwrap_or_else(Config::default_path);
    // `--status`: also open the status window at launch.
    let open_status = std::env::args().any(|a| a == "--status");

    let app = tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![
            get_status, sync_now, set_paused, reconnect, allow_deletions, open_folder, open_config, sign_in, take_pending,
            setup_preview, setup_apply, choose_setup_file, add_preview, add_apply, choose_folder, remove_library,
            login_item_info, set_login_item, dismiss_notice, cli_info, cli_install, cli_add_to_path, cli_remove, cli_repair
        ])
        .setup(move |app| {
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);
            #[cfg(unix)]
            cli::repair_at_launch(&data);
            let running = tauri::async_runtime::block_on(start(config_path.clone(), data.clone()));
            let initial = running.svc.as_ref().map(|s| s.status());
            app.manage(AppState {
                config_path: config_path.clone(),
                data_dir: data.clone(),
                rt: RwLock::new(running),
                signing_in: Mutex::new(false),
                pending: Mutex::new(None),
                config_problem: Mutex::new(None),
                notice: Mutex::new(None),
                last_add: Mutex::new(None),
            });
            let menu = build_menu(app.handle(), initial.as_ref())?;
            let icon = initial.as_ref().map(icon_key).unwrap_or((Health::Attention, false));
            TrayIconBuilder::with_id(TRAY_ID)
                .icon(tray_icon(icon))
                .icon_as_template(cfg!(target_os = "macos"))
                .tooltip(tooltip(app.handle()))
                .menu(&menu)
                .show_menu_on_left_click(true)
                .on_menu_event(|app, ev| on_menu(app, ev.id().as_ref()))
                .build(app)?;
            tauri::async_runtime::spawn(forward_events(app.handle().clone(), 0));
            watch_config(app.handle().clone());
            if open_status && let Err(e) = show_status_window(app.handle()) {
                tracing::warn!("opening status window: {e}");
            }
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("starting the tray app");

    app.run(|app, event| match event {
        // Closing the last window must not quit a tray app; only "Quit" (which passes an exit code) does.
        RunEvent::ExitRequested { api, code: None, .. } => api.prevent_exit(),
        // A setup file opened in Finder, or a looper-sharepoint:// link clicked anywhere.
        #[cfg(any(target_os = "macos", target_os = "ios"))]
        RunEvent::Opened { urls } => {
            for u in urls {
                opened(app, u.as_str());
            }
        }
        _ => {}
    });
}

fn init_logging(data: &Path) {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,hyper=warn,reqwest=warn"));
    let file = std::fs::create_dir_all(data.join("logs"))
        .and_then(|_| std::fs::OpenOptions::new().create(true).append(true).open(data.join("logs").join("tray.log")));
    match file {
        Ok(f) => tracing_subscriber::fmt().with_env_filter(filter).with_ansi(false).with_writer(Mutex::new(f)).init(),
        Err(_) => tracing_subscriber::fmt().with_env_filter(filter).with_writer(std::io::stderr).init(),
    }
}

/// Load config and start the service. Failures become an error state shown in the tray, not a crash.
async fn start(config_path: PathBuf, data: PathBuf) -> Running {
    let mut rt = Running::default();
    if !config_path.exists() {
        rt.error = Some("Not set up yet: open the setup file or link you were sent, or choose Set Up….".into());
        return rt;
    }
    let config = match Config::load(&config_path) {
        Ok(c) => c,
        Err(e) => {
            rt.error = Some(format!("{e}"));
            return rt;
        }
    };
    account::configure_client(&config);
    rt.config = Some(config.clone());
    let tokens = tokens(&config, &data);
    match Service::start(ServiceConfig { config, tokens, data_dir: data }) {
        Ok(svc) => {
            svc.set_config_file(&std::fs::canonicalize(&config_path).unwrap_or(config_path));
            match looper_sharepoint_ipc::serve(svc.clone()) {
                Ok(server) => rt._ipc = Some(server),
                Err(e) => tracing::warn!("control socket unavailable: {e:#}"),
            }
            rt.svc = Some(svc);
        }
        Err(e) => rt.error = Some(format!("{e:#}")),
    }
    rt
}

fn tokens(config: &Config, data: &Path) -> std::sync::Arc<dyn looper_sharepoint_engine::auth::TokenProvider> {
    match (std::env::var("LOOPER_SHAREPOINT_TEST_USERNAME"), std::env::var("LOOPER_SHAREPOINT_TEST_PASSWORD")) {
        // Test-only, as in the CLI: password sign-in for MFA-less QA accounts.
        (Ok(user), Ok(pass)) => looper_sharepoint_engine::auth::PasswordProvider::new(account::authority(config), user, pass),
        _ => account::token_provider(config, data),
    }
}

/// Stop the current service (letting a run finish, briefly) and start again from the config file: after setup, or
/// when the file appears or names another organization.
async fn restart(app: &AppHandle) {
    let st = app.state::<AppState>();
    let old = {
        let mut rt = st.rt.write().unwrap();
        let next = rt.generation + 1;
        let old = std::mem::take(&mut *rt);
        rt.generation = next;
        old
    };
    if let Some(svc) = &old.svc {
        let _ = tokio::time::timeout(Duration::from_secs(15), svc.shutdown()).await;
    }
    // Releases the host lock and the control socket before the new service takes them.
    drop(old);
    let mut new = start(st.config_path.clone(), st.data_dir.clone()).await;
    let generation = {
        let mut rt = st.rt.write().unwrap();
        new.generation = rt.generation;
        *rt = new;
        rt.generation
    };
    *st.config_problem.lock().unwrap() = None;
    tauri::async_runtime::spawn(forward_events(app.clone(), generation));
    refresh_menu(app);
    let _ = app.emit("status-changed", ());
}

/// The config file was saved (by the user, `add`, `remove` or `setup`): reload it. The service skips a reload when
/// the text didn't change, so a `reload` request from the CLI for the same change doesn't count twice.
async fn config_changed(app: &AppHandle) {
    let st = app.state::<AppState>();
    let Some(svc) = st.svc() else {
        restart(app).await;
        return;
    };
    match svc.reload_file().await {
        Ok(()) => *st.config_problem.lock().unwrap() = None,
        Err(e) if format!("{e:#}").contains("different organization") => {
            restart(app).await;
            return;
        }
        Err(e) => {
            tracing::warn!("config not reloaded: {e:#}");
            *st.config_problem.lock().unwrap() = Some(format!("{e:#}"));
        }
    }
    refresh_menu(app);
    let _ = app.emit("status-changed", ());
}

/// Watch the config file's folder; a change to the file (written in place or renamed over) reloads it.
fn watch_config(app: AppHandle) {
    use notify::Watcher;
    let path = app.state::<AppState>().config_path.clone();
    let Some(dir) = path.parent().map(Path::to_path_buf) else { return };
    let _ = std::fs::create_dir_all(&dir);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let name = path.file_name().map(|n| n.to_owned());
    let mut watcher = match notify::recommended_watcher(move |ev: notify::Result<notify::Event>| {
        if let Ok(ev) = ev
            && ev.paths.iter().any(|p| p.file_name() == name.as_deref())
        {
            let _ = tx.send(());
        }
    }) {
        Ok(w) => w,
        Err(e) => return tracing::warn!("can't watch the config file: {e}"),
    };
    if let Err(e) = watcher.watch(&dir, notify::RecursiveMode::NonRecursive) {
        return tracing::warn!("can't watch {}: {e}", dir.display());
    }
    tauri::async_runtime::spawn(async move {
        let _keep = watcher;
        while rx.recv().await.is_some() {
            // Wait until it's been quiet a moment (editors save in several steps).
            while tokio::time::timeout(CONFIG_QUIET, rx.recv()).await.is_ok_and(|m| m.is_some()) {}
            config_changed(&app).await;
        }
    });
}

/// Re-render the tray and notify the status window whenever the service reports a change (coalesced), and every
/// [`MENU_REFRESH`] so relative times stay fresh. Ends when the service is replaced (`generation` moves on).
async fn forward_events(app: AppHandle, generation: u64) {
    let Some(mut events) = app.state::<AppState>().svc().map(|s| s.subscribe()) else { return };
    let mut last_icon = None;
    let mut tick = tokio::time::interval(MENU_REFRESH);
    loop {
        let mut progress_only = tokio::select! {
            ev = events.recv() => match ev {
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                Ok(Event::RunFinished { record }) if record.outcome == Outcome::Ok => {
                    first_sync_done(&app);
                    false
                }
                ev => matches!(ev, Ok(Event::Progress { .. })),
            },
            _ = tick.tick() => false,
        };
        // Coalesce bursts (a large sync emits an event per file), remembering whether anything but progress came in.
        tokio::time::sleep(Duration::from_millis(300)).await;
        while let Ok(ev) = events.try_recv() {
            progress_only &= matches!(ev, Event::Progress { .. });
        }
        if app.state::<AppState>().generation() != generation {
            return;
        }
        let _ = app.emit("status-changed", ());
        if progress_only {
            continue;
        }
        let Some(svc) = app.state::<AppState>().svc() else { return };
        let status = svc.status();
        if let Some(tray) = app.tray_by_id(TRAY_ID) {
            if let Ok(menu) = build_menu(&app, Some(&status)) {
                let _ = tray.set_menu(Some(menu));
            }
            let _ = tray.set_tooltip(Some(tooltip(&app)));
            let icon = icon_key(&status);
            if last_icon != Some(icon) {
                let _ = tray.set_icon(Some(tray_icon(icon)));
                let _ = tray.set_icon_as_template(cfg!(target_os = "macos"));
                last_icon = Some(icon);
            }
        }
    }
}

/// After the first successful sync, start at login (once, unless the user turned it off): otherwise "it keeps
/// itself in sync" stops at the next restart. `specs/onboarding-design.md` §8.
fn first_sync_done(app: &AppHandle) {
    #[cfg(target_os = "macos")]
    {
        use looper_sharepoint_engine::login_item::{self, AppItem};
        let st = app.state::<AppState>();
        let mut prefs = login_item::load_prefs(&st.data_dir);
        if prefs.auto_enabled_at.is_some() || prefs.user_turned_off || login_item::app_unavailable().is_some() {
            return;
        }
        if login_item::app_status() == AppItem::Off {
            match login_item::set_app(true) {
                Ok(_) => {
                    *st.notice.lock().unwrap() = Some(
                        "Looper SharePoint now starts when you log in, so syncing carries on after a restart. Turn it off \
                         with Start at Login in the menu, or in System Settings → General → Login Items."
                            .into(),
                    );
                }
                Err(e) => return tracing::warn!("couldn't turn on start at login: {e:#}"),
            }
        }
        prefs.auto_enabled_at = Some(Utc::now().to_rfc3339());
        let _ = login_item::save_prefs(&st.data_dir, &prefs);
        refresh_menu(app);
    }
    #[cfg(not(target_os = "macos"))]
    let _ = app;
}

/// A file or link opened from outside: show it in the status window, which asks before doing anything.
fn opened(app: &AppHandle, url: &str) {
    let pending = if let Ok(Some(link)) = links::parse_app_link(url) {
        match link {
            AppLink::Setup { .. } => Pending::Setup { source: url.to_string() },
            AppLink::Add { .. } => Pending::Add { link: url.to_string() },
        }
    } else if let Some(path) = url::Url::parse(url).ok().filter(|u| u.scheme() == "file").and_then(|u| u.to_file_path().ok()) {
        Pending::Setup { source: path.display().to_string() }
    } else {
        tracing::warn!(url, "opened with something this app doesn't handle");
        return;
    };
    tracing::info!(url, "opened from outside");
    *app.state::<AppState>().pending.lock().unwrap() = Some(pending.clone());
    if show_status_window_at(app, "onboard").is_ok() {
        let _ = app.emit("show-onboard", pending);
    }
}

/// The icon to show: health, plus whether an otherwise-fine service is polling because live updates are down (an
/// outlined cloud). Live updates turned off in config is a choice, not a problem, so it keeps the normal icon.
fn icon_key(s: &Status) -> (Health, bool) {
    let health = s.health();
    let polling = matches!(s.mode(), Mode::Polling { .. } | Mode::PartlyLive { .. })
        && s.libraries.iter().any(|l| matches!(l.push, PushState::Down { retry_at: Some(_), .. } | PushState::Unavailable { .. }));
    (health, health == Health::Ok && polling)
}

fn tray_icon((h, polling): (Health, bool)) -> Image<'static> {
    let bytes: &[u8] = match (cfg!(target_os = "macos"), h) {
        (true, Health::Ok) if polling => include_bytes!("../icons/tray-polling-template.png"),
        (false, Health::Ok) if polling => include_bytes!("../icons/tray-polling.png"),
        (true, Health::Ok) => include_bytes!("../icons/tray-ok-template.png"),
        (true, Health::Syncing) => include_bytes!("../icons/tray-syncing-template.png"),
        (true, Health::Waiting) => include_bytes!("../icons/tray-waiting-template.png"),
        (true, Health::Attention) => include_bytes!("../icons/tray-attention-template.png"),
        (false, Health::Ok) => include_bytes!("../icons/tray-ok.png"),
        (false, Health::Syncing) => include_bytes!("../icons/tray-syncing.png"),
        (false, Health::Waiting) => include_bytes!("../icons/tray-waiting.png"),
        (false, Health::Attention) => include_bytes!("../icons/tray-attention.png"),
    };
    Image::from_bytes(bytes).expect("embedded tray icon")
}

fn push_glyph(p: &PushState) -> &'static str {
    match p {
        PushState::Live { .. } => "●",
        PushState::Connecting { .. } => "◌",
        PushState::Down { .. } | PushState::Unavailable { .. } => "◐",
        PushState::Off => "○",
    }
}

/// The menu's second line: live or polling, e.g. "◐ Live updates down (2m) · checking every 45s".
fn mode_menu_text(s: &Status) -> String {
    let mode = s.mode();
    let text = mode.describe(s.poll_interval_secs);
    match &mode {
        Mode::Live => format!("● {text}"),
        Mode::Connecting => format!("◌ {text}"),
        Mode::Off => format!("○ {text}"),
        Mode::PartlyLive { .. } => format!("◐ {text}"),
        Mode::Polling { since, .. } => {
            format!("◐ {}", text.replacen("down", &format!("down ({})", ago(Utc::now() - *since)), 1))
        }
    }
}

/// The product name from `account.app_name`, or the bundle's name when the config doesn't set one.
fn app_name(st: &AppState) -> String {
    match st.config() {
        Some(c) if c.account.app_name != looper_sharepoint_engine::auth::net::DEFAULT_NAME => c.account.app_name.clone(),
        _ => "Looper SharePoint".into(),
    }
}

fn tooltip(app: &AppHandle) -> String {
    let st = app.state::<AppState>();
    let name = app_name(&st);
    match (st.svc(), st.error()) {
        (Some(svc), _) => {
            let s = svc.status();
            format!("{name}: {}\n{}", s.headline(), s.mode().describe(s.poll_interval_secs))
        }
        (None, Some(e)) => format!("{name}: {}", first_line(&e)),
        (None, None) => name,
    }
}

fn build_menu(app: &AppHandle, status: Option<&Status>) -> tauri::Result<Menu<tauri::Wry>> {
    let st = app.state::<AppState>();
    let mut m = MenuBuilder::new(app);
    match status {
        None => {
            let msg = st.error().unwrap_or_else(|| "Not running".into());
            m = m.item(&MenuItemBuilder::with_id("noop", truncate(first_line(&msg), 70)).enabled(false).build(app)?);
            if st.config().is_none() {
                m = m.separator().item(&MenuItemBuilder::with_id("setup", "Set Up…").build(app)?);
            }
        }
        Some(s) => {
            let now = Utc::now();
            m = m
                .item(&MenuItemBuilder::with_id("noop", s.headline()).enabled(false).build(app)?)
                .item(&MenuItemBuilder::with_id("noop-mode", mode_menu_text(s)).enabled(false).build(app)?)
                .separator();
            for l in &s.libraries {
                let detail = match &l.state {
                    LibraryState::Syncing { done, total, .. } if *total > 0 => format!("syncing {done}/{total}"),
                    LibraryState::Syncing { .. } => "syncing…".into(),
                    LibraryState::Error => format!("failing: {}", l.last_failure.as_ref().map(|f| first_line(&f.error)).unwrap_or("")),
                    LibraryState::SignInRequired => "sign-in required".into(),
                    LibraryState::Refused => "refused: too many deletions".into(),
                    LibraryState::Idle => match l.last_success_at {
                        Some(t) => format!("synced {}", ago(now - t)),
                        None => "not synced yet".into(),
                    },
                };
                let label = truncate(&format!("{} {} — {detail}", push_glyph(&l.push), l.name), 70);
                m = m.item(&MenuItemBuilder::with_id(format!("open:{}", l.name), label).build(app)?);
            }
            if s.libraries.is_empty() {
                m = m.item(&MenuItemBuilder::with_id("noop-empty", "Nothing to sync yet").enabled(false).build(app)?);
            }
            m = m.item(&MenuItemBuilder::with_id("add", "Add a SharePoint Folder…").build(app)?);
            m = m.separator().item(&MenuItemBuilder::with_id("sync", "Sync Now").enabled(!s.libraries.is_empty()).build(app)?);
            if s.libraries.iter().any(|l| l.state == LibraryState::Refused) {
                m = m.item(&MenuItemBuilder::with_id("allow-deletions", "Allow Deletions and Sync…").build(app)?);
            }
            if s.libraries.iter().any(|l| matches!(l.push, PushState::Down { retry_at: Some(_), .. } | PushState::Unavailable { .. })) {
                m = m.item(&MenuItemBuilder::with_id("reconnect", "Reconnect Live Updates").build(app)?);
            }
            m = if s.paused {
                m.item(&MenuItemBuilder::with_id("resume", "Resume Syncing").build(app)?)
            } else {
                m.item(&MenuItemBuilder::with_id("pause", "Pause Syncing").build(app)?)
            };
            if s.libraries.iter().any(|l| l.state == LibraryState::SignInRequired) || s.account.is_none() {
                m = m.item(&MenuItemBuilder::with_id("signin", "Sign In…").build(app)?);
            }
        }
    }
    m = m.separator().item(&MenuItemBuilder::with_id("status", "Status…").build(app)?);
    #[cfg(target_os = "macos")]
    {
        use looper_sharepoint_engine::login_item::{self, AppItem};
        let item = match login_item::app_unavailable() {
            Some(_) => CheckMenuItemBuilder::with_id("login-item", "Start at Login").enabled(false).checked(false).build(app)?,
            None => CheckMenuItemBuilder::with_id("login-item", "Start at Login").checked(login_item::app_status() != AppItem::Off).build(app)?,
        };
        m = m.item(&item);
    }
    m.item(&MenuItemBuilder::with_id("config", "Open Config File").build(app)?)
        .item(&cli_menu_item(app, &st)?)
        .separator()
        .item(&MenuItemBuilder::with_id("quit", format!("Quit {}", app_name(&st))).build(app)?)
        .build()
}

/// Install / Remove / Repair Command Line Tool…, by whether a recorded link exists (specs/install-cli-design.md §4.1).
fn cli_menu_item(app: &AppHandle, st: &AppState) -> tauri::Result<tauri::menu::MenuItem<tauri::Wry>> {
    #[cfg(unix)]
    {
        let (id, label, enabled) = match looper_sharepoint_engine::cli_link::load_record(&st.data_dir) {
            Some(r) if looper_sharepoint_engine::cli_link::repair(&r) == looper_sharepoint_engine::cli_link::Repair::NeedsAdmin => {
                ("cli-repair", "Repair Command Line Tool…", true)
            }
            Some(_) => ("cli-remove", "Remove Command Line Tool…", true),
            None => ("cli-install", "Install Command Line Tool…", cli::find().is_ok()),
        };
        MenuItemBuilder::with_id(id, label).enabled(enabled).build(app)
    }
    #[cfg(not(unix))]
    {
        let _ = st;
        MenuItemBuilder::with_id("noop-cli", "Install Command Line Tool…").enabled(false).build(app)
    }
}

/// The tray menu was rebuilt from outside a status event (the command line tool was installed or removed, the
/// config changed, ...).
fn refresh_menu(app: &AppHandle) {
    let st = app.state::<AppState>();
    let status = st.svc().map(|s| s.status());
    if let Some(tray) = app.tray_by_id(TRAY_ID) {
        if let Ok(menu) = build_menu(app, status.as_ref()) {
            let _ = tray.set_menu(Some(menu));
        }
        let _ = tray.set_tooltip(Some(tooltip(app)));
    }
}

fn on_menu(app: &AppHandle, id: &str) {
    let st = app.state::<AppState>();
    let svc = st.svc();
    let result = match id {
        "sync" => svc.as_ref().map(|s| s.trigger(None, Trigger::Manual)).transpose().map(|_| ()).map_err(|e| e.to_string()),
        "reconnect" => svc.as_ref().map(|s| s.reconnect_push(None)).transpose().map(|_| ()).map_err(|e| e.to_string()),
        // The refusal explains what would be deleted; the status window shows it next to this same action.
        "allow-deletions" => show_status_window(app),
        "pause" => {
            if let Some(s) = &svc {
                s.pause();
            }
            Ok(())
        }
        "resume" => {
            if let Some(s) = &svc {
                s.resume();
            }
            Ok(())
        }
        "signin" => {
            start_sign_in(app.clone());
            Ok(())
        }
        "status" => show_status_window(app),
        "config" => open_config_file(app, &st),
        // The onboarding and command line tool dialogs live in the status window: open it there (the hash covers a
        // window that isn't loaded yet).
        "setup" | "add" => show_status_window_at(app, "onboard").map(|_| {
            let _ = app.emit("show-onboard", serde_json::json!({ "kind": id }));
        }),
        "login-item" => {
            #[cfg(target_os = "macos")]
            {
                let on = looper_sharepoint_engine::login_item::app_status() == looper_sharepoint_engine::login_item::AppItem::Off;
                set_login_item_now(&st, on).map(|_| refresh_menu(app))
            }
            #[cfg(not(target_os = "macos"))]
            Ok(())
        }
        "cli-install" | "cli-remove" | "cli-repair" => show_status_window_at(app, id).map(|_| {
            let _ = app.emit("show-cli", &id[4..]);
        }),
        "quit" => {
            let app = app.clone();
            tauri::async_runtime::spawn(async move {
                if let Some(svc) = app.state::<AppState>().svc() {
                    // Let a run in progress finish, but don't hang forever.
                    let _ = tokio::time::timeout(Duration::from_secs(15), svc.shutdown()).await;
                }
                app.exit(0);
            });
            Ok(())
        }
        other if other.starts_with("open:") => open_library_folder(app, &st, &other[5..]),
        _ => Ok(()),
    };
    if let Err(e) = result {
        tracing::warn!(menu = id, "{e}");
    }
}

fn show_status_window(app: &AppHandle) -> Result<(), String> {
    show_status_window_at(app, "")
}

/// Show the status window; `hash` (if any) names a dialog to open in it when the window is first created
/// (`onboard`, `cli-install`, ...). An open window gets an event instead.
fn show_status_window_at(app: &AppHandle, hash: &str) -> Result<(), String> {
    // A menu-bar-only (accessory) app can't bring a window to the front; become a regular app while a window is
    // open, and go back to menu-bar-only when it closes.
    #[cfg(target_os = "macos")]
    let _ = app.set_activation_policy(tauri::ActivationPolicy::Regular);
    if let Some(w) = app.get_webview_window(STATUS_WINDOW) {
        let _ = w.show();
        let _ = w.unminimize();
        return w.set_focus().map_err(|e| e.to_string());
    }
    let page = if hash.is_empty() { "index.html".to_string() } else { format!("index.html#{hash}") };
    let w = WebviewWindowBuilder::new(app, STATUS_WINDOW, WebviewUrl::App(page.into()))
        .title(app_name(&app.state::<AppState>()))
        .inner_size(820.0, 600.0)
        .min_inner_size(560.0, 360.0)
        .focused(true)
        .build()
        .map_err(|e| e.to_string())?;
    #[cfg(target_os = "macos")]
    {
        let handle = app.clone();
        w.on_window_event(move |ev| {
            if let tauri::WindowEvent::Destroyed = ev {
                let _ = handle.set_activation_policy(tauri::ActivationPolicy::Accessory);
            }
        });
    }
    w.set_focus().map_err(|e| e.to_string())
}

fn open_library_folder(app: &AppHandle, st: &AppState, name: &str) -> Result<(), String> {
    let cfg = st.config().ok_or("no config")?;
    let lib = cfg.library(name).ok_or("no such library")?;
    let path = cfg.local_root(lib);
    std::fs::create_dir_all(&path).map_err(|e| e.to_string())?;
    app.opener().open_path(path.to_string_lossy(), None::<&str>).map_err(|e| e.to_string())
}

/// Open config.toml in the default editor, creating it from the example first if it doesn't exist.
fn open_config_file(app: &AppHandle, st: &AppState) -> Result<(), String> {
    let p = &st.config_path;
    if !p.exists() {
        if let Some(dir) = p.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        std::fs::write(p, include_str!("../../../config.example.toml")).map_err(|e| e.to_string())?;
    }
    #[cfg(target_os = "macos")]
    {
        let _ = app;
        // .toml has no default handler on a stock Mac; open it as text.
        std::process::Command::new("open").arg("-t").arg(p).spawn().map(|_| ()).map_err(|e| e.to_string())
    }
    #[cfg(not(target_os = "macos"))]
    {
        app.opener().open_path(p.to_string_lossy(), None::<&str>).map_err(|e| e.to_string())
    }
}

fn start_sign_in(app: AppHandle) {
    let st = app.state::<AppState>();
    let Some(cfg) = st.config() else { return };
    {
        let mut busy = st.signing_in.lock().unwrap();
        if *busy {
            return;
        }
        *busy = true;
    }
    let data = st.data_dir.clone();
    tauri::async_runtime::spawn(async move {
        let opener = app.clone();
        let r = account::sign_in(&cfg, &data, Some(AuthFlow::Pkce), |p| {
            if let SignInPrompt::OpenUrl(url) = p {
                let _ = opener.opener().open_url(url, None::<&str>);
            }
        })
        .await;
        let st = app.state::<AppState>();
        *st.signing_in.lock().unwrap() = false;
        match r {
            Ok(a) => {
                tracing::info!(user = a.username, "signed in");
                if let Some(svc) = st.svc() {
                    svc.signed_in();
                }
            }
            Err(e) => tracing::warn!("sign-in failed: {e:#}"),
        }
        let _ = app.emit("status-changed", ());
        let _ = app.emit("signed-in", ());
    });
}

#[cfg(target_os = "macos")]
fn set_login_item_now(st: &AppState, on: bool) -> Result<(), String> {
    use looper_sharepoint_engine::login_item;
    let state = login_item::set_app(on).map_err(|e| format!("{e:#}"))?;
    let mut prefs = login_item::load_prefs(&st.data_dir);
    prefs.user_turned_off = !on;
    let _ = login_item::save_prefs(&st.data_dir, &prefs);
    if state == login_item::AppItem::NeedsApproval {
        login_item::open_settings();
    }
    Ok(())
}

// ---- commands for the status window ----

#[tauri::command]
fn get_status(state: State<'_, AppState>) -> Snapshot {
    let svc = state.svc();
    let status = svc.as_ref().map(Service::status);
    let error = state.error();
    let headline = match (&status, &error) {
        (Some(s), _) if s.libraries.is_empty() => "Nothing to sync yet".into(),
        (Some(s), _) => s.headline(),
        (None, Some(e)) => first_line(e).to_string(),
        (None, None) => "Not running".into(),
    };
    let mode = status.as_ref().filter(|s| !s.libraries.is_empty()).map(Status::mode);
    let mode_text = status.as_ref().filter(|s| !s.libraries.is_empty()).map(|s| s.mode().describe(s.poll_interval_secs));
    Snapshot {
        history: svc.as_ref().map(|s| s.history(50)).unwrap_or_default(),
        status,
        error,
        config_path: state.config_path.display().to_string(),
        headline,
        mode_text,
        mode,
        set_up: state.config().is_some(),
        config_problem: state.config_problem.lock().unwrap().clone(),
        notice: state.notice.lock().unwrap().clone(),
    }
}

#[tauri::command]
fn dismiss_notice(state: State<'_, AppState>) {
    *state.notice.lock().unwrap() = None;
}

#[tauri::command]
fn sync_now(state: State<'_, AppState>, library: Option<String>) -> Result<(), String> {
    let svc = state.svc().ok_or("the sync service is not running")?;
    svc.trigger(library.as_deref(), Trigger::Manual).map_err(|e| format!("{e:#}"))
}

#[tauri::command]
fn reconnect(state: State<'_, AppState>, library: Option<String>) -> Result<(), String> {
    let svc = state.svc().ok_or("the sync service is not running")?;
    svc.reconnect_push(library.as_deref()).map_err(|e| format!("{e:#}"))
}

/// Re-run the refused libraries with the mass-delete guard overridden (the user saw what would be deleted).
#[tauri::command]
fn allow_deletions(state: State<'_, AppState>, library: Option<String>) -> Result<(), String> {
    let svc = state.svc().ok_or("the sync service is not running")?;
    let refused: Vec<String> = svc
        .status()
        .libraries
        .into_iter()
        .filter(|l| l.state == LibraryState::Refused && library.as_deref().is_none_or(|n| n.eq_ignore_ascii_case(&l.name)))
        .map(|l| l.name)
        .collect();
    tauri::async_runtime::spawn(async move {
        for lib in refused {
            let opts = RunOptions { allow_mass_delete: true, ..Default::default() };
            if let Err(e) = svc.sync_now(Some(&lib), opts).await {
                tracing::warn!(library = lib, "allowed run failed: {e:#}");
            }
        }
    });
    Ok(())
}

#[tauri::command]
fn set_paused(state: State<'_, AppState>, paused: bool) -> Result<(), String> {
    let svc = state.svc().ok_or("the sync service is not running")?;
    if paused { svc.pause() } else { svc.resume() }
    Ok(())
}

#[tauri::command]
fn open_folder(app: AppHandle, state: State<'_, AppState>, library: String) -> Result<(), String> {
    open_library_folder(&app, &state, &library)
}

#[tauri::command]
fn open_config(app: AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    open_config_file(&app, &state)
}

#[tauri::command]
fn sign_in(app: AppHandle) {
    start_sign_in(app);
}

// ---- setting up and adding (specs/onboarding-design.md §4, §5) ----

/// What was opened from outside, if anything (once).
#[tauri::command]
fn take_pending(state: State<'_, AppState>) -> Option<Pending> {
    state.pending.lock().unwrap().take()
}

#[tauri::command]
fn setup_preview(state: State<'_, AppState>, source: String) -> Result<SetupPlan, String> {
    onboard::plan_setup(&state.config_path, source.trim()).map_err(|e| format!("{e:#}"))
}

#[derive(Serialize)]
struct SetupResult {
    #[serde(flatten)]
    done: SetupDone,
    signed_in: bool,
}

/// Apply a setup the user confirmed, start (or reload) the service, and sign in if needed.
#[tauri::command]
async fn setup_apply(app: AppHandle, source: String, replace: bool) -> Result<SetupResult, String> {
    let st = app.state::<AppState>();
    let plan = onboard::plan_setup(&st.config_path, source.trim()).map_err(|e| format!("{e:#}"))?;
    let done = plan.apply(&st.config_path, replace).map_err(|e| format!("{e:#}"))?;
    if done.switched || st.svc().is_none() {
        restart(&app).await;
    } else {
        config_changed(&app).await;
    }
    let signed_in = st.config().is_some_and(|c| account::token_provider(&c, &st.data_dir).account().ok().flatten().is_some())
        || std::env::var("LOOPER_SHAREPOINT_TEST_USERNAME").is_ok();
    if !signed_in {
        start_sign_in(app.clone());
    }
    Ok(SetupResult { done, signed_in })
}

#[tauri::command]
async fn choose_setup_file(app: AppHandle) -> Option<String> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.dialog()
        .file()
        .set_title("Choose the setup file")
        .add_filter("Setup file", &[looper_sharepoint_engine::core::setup::EXTENSION, "toml"])
        .pick_file(move |f| {
            let _ = tx.send(f.and_then(|f| f.into_path().ok()));
        });
    rx.await.ok().flatten().map(|p| p.display().to_string())
}

/// A Graph client for the signed-in account.
fn graph_for(st: &AppState) -> Result<(Config, GraphClient), String> {
    let cfg = st.config().ok_or("not set up yet: set up first")?;
    if std::env::var("LOOPER_SHAREPOINT_TEST_USERNAME").is_err() && account::token_provider(&cfg, &st.data_dir).account().ok().flatten().is_none() {
        return Err("sign in first".into());
    }
    let tokens = tokens(&cfg, &st.data_dir);
    let graph = GraphClient::with_rate(tokens, cfg.sync.max_requests_per_second);
    Ok((cfg, graph))
}

#[tauri::command]
async fn add_preview(state: State<'_, AppState>, link: String, library: Option<String>) -> Result<AddPlan, String> {
    let (cfg, graph) = graph_for(&state)?;
    let plan = onboard::plan_add(&graph, &cfg, link.trim(), library.as_deref()).await.map_err(|e| format!("{e:#}"))?;
    *state.last_add.lock().unwrap() = Some((format!("{}\n{}", link.trim(), library.unwrap_or_default()), plan.clone()));
    Ok(plan)
}

#[derive(Serialize)]
struct Added {
    name: String,
    path: String,
}

#[tauri::command]
async fn add_apply(
    app: AppHandle,
    link: String,
    library: Option<String>,
    to: String,
    read_only: bool,
    adopt: bool,
) -> Result<Added, String> {
    let st = app.state::<AppState>();
    let key = format!("{}\n{}", link.trim(), library.clone().unwrap_or_default());
    let cached = st.last_add.lock().unwrap().clone().filter(|(k, _)| *k == key).map(|(_, p)| p);
    let plan = match cached {
        Some(p) => p,
        None => {
            let (cfg, graph) = graph_for(&st)?;
            onboard::plan_add(&graph, &cfg, link.trim(), library.as_deref()).await.map_err(|e| format!("{e:#}"))?
        }
    };
    let opts = AddOptions { to: PathBuf::from(to), name: None, read_only, adopt };
    let (cfg, name) = plan.apply(&st.config_path, &opts).map_err(|e| format!("{e:#}"))?;
    *st.last_add.lock().unwrap() = None;
    config_changed(&app).await;
    let path = cfg.library(&name).map(|l| cfg.local_root(l).display().to_string()).unwrap_or_default();
    Ok(Added { name, path })
}

#[tauri::command]
async fn choose_folder(app: AppHandle, start: Option<String>) -> Option<String> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let mut d = app.dialog().file().set_title("Choose the local folder");
    if let Some(s) = start.map(|s| looper_sharepoint_engine::core::config::expand_tilde(&s)) {
        // Start in the nearest folder that exists.
        if let Some(dir) = s.ancestors().find(|a| a.is_dir()) {
            d = d.set_directory(dir);
        }
    }
    d.set_can_create_directories(true).pick_folder(move |f| {
        let _ = tx.send(f.and_then(|f| f.into_path().ok()));
    });
    rx.await.ok().flatten().map(|p| p.display().to_string())
}

/// Stop syncing a library. Never deletes files.
#[tauri::command]
async fn remove_library(app: AppHandle, name: String) -> Result<String, String> {
    let st = app.state::<AppState>();
    let old = st.config().ok_or("not set up")?;
    let (_, lib) = onboard::remove(&st.config_path, &name).map_err(|e| format!("{e:#}"))?;
    let path = old.local_root(&lib).display().to_string();
    config_changed(&app).await;
    if st.config_problem.lock().unwrap().is_none() {
        let _ = onboard::forget_state(&st.data_dir, lib.name());
    }
    Ok(path)
}

#[derive(Serialize)]
struct LoginItemInfo {
    on: bool,
    needs_approval: bool,
    unavailable: Option<String>,
}

#[tauri::command]
fn login_item_info() -> LoginItemInfo {
    #[cfg(target_os = "macos")]
    {
        use looper_sharepoint_engine::login_item::{self, AppItem};
        let s = login_item::app_status();
        LoginItemInfo { on: s != AppItem::Off, needs_approval: s == AppItem::NeedsApproval, unavailable: login_item::app_unavailable() }
    }
    #[cfg(not(target_os = "macos"))]
    LoginItemInfo { on: false, needs_approval: false, unavailable: Some("not supported on this platform yet".into()) }
}

#[tauri::command]
fn set_login_item(app: AppHandle, state: State<'_, AppState>, on: bool) -> Result<LoginItemInfo, String> {
    #[cfg(target_os = "macos")]
    set_login_item_now(&state, on)?;
    #[cfg(not(target_os = "macos"))]
    let _ = (&state, on);
    refresh_menu(&app);
    Ok(login_item_info())
}

// ---- the command line tool (specs/install-cli-design.md) ----

#[cfg(unix)]
mod cli {
    use std::path::{Path, PathBuf};

    use looper_sharepoint_engine::cli_link::{self, Action, PathFix, Record, Repair, Scope};
    use serde::Serialize;

    /// The CLI next to the tray's own executable, checked (§3).
    pub fn find() -> Result<PathBuf, String> {
        let me = std::env::current_exe().and_then(std::fs::canonicalize).map_err(|e| format!("finding the app: {e}"))?;
        let cli = me.with_file_name(cli_link::NAME);
        let cli = std::fs::canonicalize(&cli).unwrap_or(cli);
        cli_link::check_cli(&cli)?;
        let out = std::process::Command::new(&cli).arg("--version").output().map_err(|e| format!("running {}: {e}", cli.display()))?;
        let version = String::from_utf8_lossy(&out.stdout);
        if !version.split_whitespace().any(|w| w == env!("CARGO_PKG_VERSION")) {
            return Err(format!("{} is a different version ({}) from this app ({})", cli.display(), version.trim(), env!("CARGO_PKG_VERSION")));
        }
        Ok(cli)
    }

    /// §6: re-point our link if its target is gone and that needs no password; forget it if it was replaced.
    pub fn repair_at_launch(data: &Path) {
        let Some(rec) = cli_link::load_record(data) else { return };
        match cli_link::repair(&rec) {
            Repair::Fine | Repair::NeedsAdmin => {}
            Repair::Forget => {
                tracing::info!(link = %rec.link.display(), "command line link replaced or removed; forgetting it");
                let _ = cli_link::clear_record(data);
            }
            Repair::Repoint => match find() {
                Ok(cli) => match cli_link::link_unprivileged(&cli, &rec.link) {
                    Ok(()) => {
                        tracing::info!(link = %rec.link.display(), to = %cli.display(), "repaired the command line link");
                        let _ = cli_link::save_record(data, &Record { target: cli, ..rec });
                    }
                    Err(e) => tracing::warn!("repairing the command line link: {e:#}"),
                },
                Err(e) => tracing::warn!("command line link is broken, and can't be repaired: {e}"),
            },
        }
    }

    #[derive(Serialize)]
    pub struct Info {
        /// The CLI that would be linked, or why there's none.
        cli: Option<String>,
        problem: Option<String>,
        dev: bool,
        admin: bool,
        /// "All users" needs the administrator prompt.
        system_needs_admin: bool,
        installed: Option<Record>,
        /// Another `looper-sharepoint` a new terminal runs first.
        shadowed_by: Option<String>,
    }

    pub fn info(data: &Path) -> Info {
        let found = find();
        let installed = cli_link::load_record(data);
        let shadowed_by = installed.as_ref().and_then(|r| {
            let path = cli_link::login_shell_path(&cli_link::user_shell())?;
            cli_link::shadowed_by(&path, &r.link).map(|p| p.display().to_string())
        });
        Info {
            dev: found.as_ref().is_ok_and(|c| cli_link::location(c) == cli_link::Location::Dev),
            cli: found.as_ref().ok().map(|c| c.display().to_string()),
            problem: found.err(),
            admin: cli_link::is_admin(),
            system_needs_admin: !cli_link::writable(Path::new(cli_link::SYSTEM_DIR)),
            installed,
            shadowed_by,
        }
    }

    #[derive(Serialize, Default)]
    pub struct Installed {
        /// The administrator prompt was cancelled: nothing changed.
        cancelled: bool,
        already: bool,
        link: String,
        /// "Just me", and `~/.local/bin` isn't on the login shell's PATH.
        needs_path: bool,
        /// Where the PATH line would go ("fish" for fish_add_path), or None for a shell we can't edit.
        path_target: Option<String>,
        /// The PATH check couldn't run (timeout): the line is only shown.
        path_unknown: bool,
        line: String,
        shadowed_by: Option<String>,
    }

    pub fn install(data: &Path, scope: Scope, app_name: &str) -> Result<Installed, String> {
        let cli = find()?;
        let home = cli_link::home().map_err(|e| e.to_string())?;
        let link = scope.link(&home);
        let record = cli_link::load_record(data);
        let mut out = Installed { link: link.display().to_string(), line: cli_link::PATH_LINE.into(), ..Default::default() };
        match cli_link::decide(&cli_link::inspect(&link, &cli, record.as_ref()), &link, false) {
            Action::Stop(why) => return Err(why),
            Action::AlreadyInstalled => out.already = true,
            Action::Create | Action::Replace => {
                if scope == Scope::System && !cli_link::writable(link.parent().unwrap_or(&link)) {
                    let why = format!("{app_name} wants to install the looper-sharepoint command for all users.");
                    if !cli_link::run_with_admin_prompt(&cli_link::privileged_link_commands(&cli, &link), &why).map_err(|e| e.to_string())? {
                        out.cancelled = true;
                        return Ok(out);
                    }
                } else {
                    cli_link::link_unprivileged(&cli, &link).map_err(|e| format!("{e:#}"))?;
                }
            }
        }
        let path_edit = record.filter(|r| r.link == link).and_then(|r| r.path_edit);
        cli_link::save_record(data, &Record { link: link.clone(), target: cli, scope, installed_at: cli_link::now(), path_edit })
            .map_err(|e| e.to_string())?;
        let shell = cli_link::user_shell();
        let path = cli_link::login_shell_path(&shell);
        out.path_unknown = path.is_none();
        if scope == Scope::User && !path.as_deref().is_some_and(|p| cli_link::path_has(p, &scope.dir(&home))) {
            out.needs_path = true;
            out.path_target = match cli_link::path_fix(&shell, &home) {
                PathFix::File(f) => Some(f.display().to_string()),
                PathFix::Fish => Some("fish".into()),
                PathFix::Manual => None,
            };
        }
        out.shadowed_by = path.as_deref().and_then(|p| cli_link::shadowed_by(p, &link)).map(|p| p.display().to_string());
        Ok(out)
    }

    pub fn add_to_path(data: &Path) -> Result<String, String> {
        let home = cli_link::home().map_err(|e| e.to_string())?;
        match cli_link::path_fix(&cli_link::user_shell(), &home) {
            PathFix::File(f) => {
                cli_link::add_path_block(&f).map_err(|e| format!("{e:#}"))?;
                if let Some(rec) = cli_link::load_record(data) {
                    let _ = cli_link::save_record(data, &Record { path_edit: Some(f.clone()), ..rec });
                }
                Ok(format!("Added ~/.local/bin to your PATH in {}", f.display()))
            }
            PathFix::Fish => cli_link::fish_add_path().map(|_| "Added ~/.local/bin to your PATH (fish_add_path)".into()).map_err(|e| e.to_string()),
            PathFix::Manual => Err(format!("Your shell isn't one this app can edit. Add this to its startup file: {}", cli_link::PATH_LINE)),
        }
    }

    #[derive(Serialize, Default)]
    pub struct Removed {
        cancelled: bool,
        removed: Option<String>,
        /// The link was no longer ours: left alone.
        left: Option<String>,
        path_removed: Option<String>,
    }

    pub fn remove(data: &Path, remove_path: bool, app_name: &str) -> Result<Removed, String> {
        let rec = cli_link::load_record(data).ok_or("the command line tool isn't installed by this app")?;
        let mut out = Removed::default();
        if cli_link::still_ours(&rec) {
            if cli_link::writable(rec.link.parent().unwrap_or(&rec.link)) {
                std::fs::remove_file(&rec.link).map_err(|e| format!("removing {}: {e}", rec.link.display()))?;
            } else {
                let why = format!("{app_name} wants to remove the looper-sharepoint command.");
                if !cli_link::run_with_admin_prompt(&cli_link::privileged_remove_commands(&rec.link), &why).map_err(|e| e.to_string())? {
                    out.cancelled = true;
                    return Ok(out);
                }
            }
            out.removed = Some(rec.link.display().to_string());
        } else {
            out.left = Some(rec.link.display().to_string());
        }
        if remove_path && let Some(f) = &rec.path_edit && cli_link::remove_path_block(f).map_err(|e| format!("{e:#}"))? {
            out.path_removed = Some(f.display().to_string());
        }
        cli_link::clear_record(data).map_err(|e| e.to_string())?;
        Ok(out)
    }

    /// "All users" link whose target is gone: re-point it, with the administrator prompt. `Ok(false)`: cancelled.
    pub fn repair(data: &Path, app_name: &str) -> Result<bool, String> {
        let rec = cli_link::load_record(data).ok_or("the command line tool isn't installed by this app")?;
        let cli = find()?;
        let why = format!("{app_name} wants to repair the looper-sharepoint command for all users.");
        if !cli_link::run_with_admin_prompt(&cli_link::privileged_link_commands(&cli, &rec.link), &why).map_err(|e| e.to_string())? {
            return Ok(false);
        }
        cli_link::save_record(data, &Record { target: cli, ..rec }).map_err(|e| e.to_string())?;
        Ok(true)
    }
}

#[cfg(unix)]
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T, String> + Send + 'static) -> Result<T, String> {
    tauri::async_runtime::spawn_blocking(f).await.map_err(|e| e.to_string())?
}

#[cfg(unix)]
#[tauri::command]
async fn cli_info(state: State<'_, AppState>) -> Result<cli::Info, String> {
    let data = state.data_dir.clone();
    blocking(move || Ok(cli::info(&data))).await
}

#[cfg(unix)]
#[tauri::command]
async fn cli_install(app: AppHandle, state: State<'_, AppState>, all_users: bool) -> Result<cli::Installed, String> {
    let (data, name) = (state.data_dir.clone(), app_name(&state));
    let scope = if all_users { looper_sharepoint_engine::cli_link::Scope::System } else { looper_sharepoint_engine::cli_link::Scope::User };
    let r = blocking(move || cli::install(&data, scope, &name)).await;
    refresh_menu(&app);
    r
}

#[cfg(unix)]
#[tauri::command]
async fn cli_add_to_path(state: State<'_, AppState>) -> Result<String, String> {
    let data = state.data_dir.clone();
    blocking(move || cli::add_to_path(&data)).await
}

#[cfg(unix)]
#[tauri::command]
async fn cli_remove(app: AppHandle, state: State<'_, AppState>, remove_path: bool) -> Result<cli::Removed, String> {
    let (data, name) = (state.data_dir.clone(), app_name(&state));
    let r = blocking(move || cli::remove(&data, remove_path, &name)).await;
    refresh_menu(&app);
    r
}

#[cfg(unix)]
#[tauri::command]
async fn cli_repair(app: AppHandle, state: State<'_, AppState>) -> Result<bool, String> {
    let (data, name) = (state.data_dir.clone(), app_name(&state));
    let r = blocking(move || cli::repair(&data, &name)).await;
    refresh_menu(&app);
    r
}

#[cfg(not(unix))]
mod cli_stubs {
    const NO: &str = "installing the command line tool isn't supported on this platform yet";
    #[tauri::command]
    pub fn cli_info() -> Result<(), String> { Err(NO.into()) }
    #[tauri::command]
    pub fn cli_install(all_users: bool) -> Result<(), String> { let _ = all_users; Err(NO.into()) }
    #[tauri::command]
    pub fn cli_add_to_path() -> Result<(), String> { Err(NO.into()) }
    #[tauri::command]
    pub fn cli_remove(remove_path: bool) -> Result<(), String> { let _ = remove_path; Err(NO.into()) }
    #[tauri::command]
    pub fn cli_repair() -> Result<(), String> { Err(NO.into()) }
}
#[cfg(not(unix))]
use cli_stubs::*;

// ---- helpers ----

fn ago(d: chrono::Duration) -> String {
    let s = d.num_seconds().max(0);
    match s {
        0..60 => "just now".into(),
        60..3600 => format!("{}m ago", s / 60),
        3600..86400 => format!("{}h ago", s / 3600),
        _ => format!("{}d ago", s / 86400),
    }
}

fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or("")
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max - 1).collect::<String>())
    }
}
