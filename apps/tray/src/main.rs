//! `looper-sharepoint-tray`: a menu-bar (macOS) / notification-area (Windows) host for the sync service.
//!
//! Like `looper-sharepoint run`, this is only a host: it embeds `looper-sharepoint-engine` through its public API,
//! renders `Status`, and forwards user actions. No windows exist until the user opens one from the tray.
//! See `specs/m2-design.md` §7.

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use chrono::Utc;
use serde::Serialize;
use looper_sharepoint_engine::account::{self, SignInPrompt};
use looper_sharepoint_engine::core::config::{AuthFlow, Config, data_dir};
use looper_sharepoint_engine::{Event, Health, LibraryState, Mode, PushState, RunOptions, RunRecord, Service, ServiceConfig, Status, Trigger};
use tauri::image::Image;
use tauri::menu::{Menu, MenuBuilder, MenuItemBuilder};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Emitter, Manager, RunEvent, State, WebviewUrl, WebviewWindowBuilder};
use tauri_plugin_opener::OpenerExt;

const TRAY_ID: &str = "main";
const STATUS_WINDOW: &str = "status";
/// Menus show relative times; refresh them at least this often.
const MENU_REFRESH: Duration = Duration::from_secs(30);

struct AppState {
    config_path: PathBuf,
    data_dir: PathBuf,
    config: Option<Config>,
    svc: Option<Service>,
    /// Why there is no service (bad config, another host running, ...).
    error: Option<String>,
    _ipc: Option<looper_sharepoint_ipc::Server>,
    signing_in: Mutex<bool>,
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
}

fn main() {
    let data = data_dir();
    init_logging(&data);
    let config_path = std::env::var_os("LOOPER_SHAREPOINT_CONFIG").map(PathBuf::from).unwrap_or_else(Config::default_path);
    // `--status`: also open the status window at launch.
    let open_status = std::env::args().any(|a| a == "--status");

    let app = tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![get_status, sync_now, set_paused, reconnect, allow_deletions, open_folder, open_config, sign_in, cli_info, cli_install, cli_add_to_path, cli_remove, cli_repair])
        .setup(move |app| {
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Accessory);
            #[cfg(unix)]
            cli::repair_at_launch(&data);
            let state = tauri::async_runtime::block_on(start(config_path.clone(), data.clone()));
            let initial = state.svc.as_ref().map(|s| s.status());
            app.manage(state);
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
            tauri::async_runtime::spawn(forward_events(app.handle().clone()));
            if open_status && let Err(e) = show_status_window(app.handle()) {
                tracing::warn!("opening status window: {e}");
            }
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("starting the tray app");

    app.run(|_, event| {
        // Closing the last window must not quit a tray app; only "Quit" (which passes an exit code) does.
        if let RunEvent::ExitRequested { api, code: None, .. } = event {
            api.prevent_exit();
        }
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
async fn start(config_path: PathBuf, data: PathBuf) -> AppState {
    let mut st = AppState {
        config_path: config_path.clone(),
        data_dir: data.clone(),
        config: None,
        svc: None,
        error: None,
        _ipc: None,
        signing_in: Mutex::new(false),
    };
    if !config_path.exists() {
        st.error = Some("Not set up yet: choose Open Config File, fill in your organization's tenant and client_id, then restart.".into());
        return st;
    }
    let config = match Config::load(&config_path) {
        Ok(c) if c.libraries.is_empty() => {
            st.error = Some("No libraries configured yet. Open the config file to add one.".into());
            st.config = Some(c);
            return st;
        }
        Ok(c) => c,
        Err(e) => {
            st.error = Some(format!("{e}"));
            return st;
        }
    };
    st.config = Some(config.clone());
    let tokens: std::sync::Arc<dyn looper_sharepoint_engine::auth::TokenProvider> =
        match (std::env::var("LOOPER_SHAREPOINT_TEST_USERNAME"), std::env::var("LOOPER_SHAREPOINT_TEST_PASSWORD")) {
            // Test-only, as in the CLI: password sign-in for MFA-less QA accounts.
            (Ok(user), Ok(pass)) => looper_sharepoint_engine::auth::PasswordProvider::new(account::authority(&config), user, pass),
            _ => account::token_provider(&config, &data),
        };
    match Service::start(ServiceConfig { config, tokens, data_dir: data }) {
        Ok(svc) => {
            svc.set_config_file(&std::fs::canonicalize(&config_path).unwrap_or(config_path));
            match looper_sharepoint_ipc::serve(svc.clone()) {
                Ok(server) => st._ipc = Some(server),
                Err(e) => tracing::warn!("control socket unavailable: {e:#}"),
            }
            st.svc = Some(svc);
        }
        Err(e) => st.error = Some(format!("{e:#}")),
    }
    st
}

/// Re-render the tray and notify the status window whenever the service reports a change (coalesced), and every
/// [`MENU_REFRESH`] so relative times stay fresh.
async fn forward_events(app: AppHandle) {
    let Some(svc) = app.state::<AppState>().svc.clone() else { return };
    let mut events = svc.subscribe();
    let mut last_icon = None;
    let mut tick = tokio::time::interval(MENU_REFRESH);
    loop {
        let mut progress_only = tokio::select! {
            ev = events.recv() => matches!(ev, Ok(Event::Progress { .. })),
            _ = tick.tick() => false,
        };
        // Coalesce bursts (a large sync emits an event per file), remembering whether anything but progress came in.
        tokio::time::sleep(Duration::from_millis(300)).await;
        while let Ok(ev) = events.try_recv() {
            progress_only &= matches!(ev, Event::Progress { .. });
        }
        let _ = app.emit("status-changed", ());
        if progress_only {
            continue;
        }
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
    match &st.config {
        Some(c) if c.account.app_name != looper_sharepoint_engine::auth::net::DEFAULT_NAME => c.account.app_name.clone(),
        _ => "Looper SharePoint".into(),
    }
}

fn tooltip(app: &AppHandle) -> String {
    let st = app.state::<AppState>();
    let name = app_name(&st);
    match (&st.svc, &st.error) {
        (Some(svc), _) => {
            let s = svc.status();
            format!("{name}: {}\n{}", s.headline(), s.mode().describe(s.poll_interval_secs))
        }
        (None, Some(e)) => format!("{name}: {}", first_line(e)),
        (None, None) => name,
    }
}

fn build_menu(app: &AppHandle, status: Option<&Status>) -> tauri::Result<Menu<tauri::Wry>> {
    let st = app.state::<AppState>();
    let mut m = MenuBuilder::new(app);
    match status {
        None => {
            let msg = st.error.as_deref().map(first_line).unwrap_or("Not running");
            m = m.item(&MenuItemBuilder::with_id("noop", truncate(msg, 70)).enabled(false).build(app)?);
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
            m = m.separator().item(&MenuItemBuilder::with_id("sync", "Sync Now").build(app)?);
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
    m.separator()
        .item(&MenuItemBuilder::with_id("status", "Status…").build(app)?)
        .item(&MenuItemBuilder::with_id("config", "Open Config File").build(app)?)
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

/// The tray menu was rebuilt from outside a status event (the command line tool was installed or removed).
fn refresh_menu(app: &AppHandle) {
    let st = app.state::<AppState>();
    let status = st.svc.as_ref().map(Service::status);
    if let (Some(tray), Ok(menu)) = (app.tray_by_id(TRAY_ID), build_menu(app, status.as_ref())) {
        let _ = tray.set_menu(Some(menu));
    }
}

fn on_menu(app: &AppHandle, id: &str) {
    let st = app.state::<AppState>();
    let result = match id {
        "sync" => st.svc.as_ref().map(|s| s.trigger(None, Trigger::Manual)).transpose().map(|_| ()).map_err(|e| e.to_string()),
        "reconnect" => st.svc.as_ref().map(|s| s.reconnect_push(None)).transpose().map(|_| ()).map_err(|e| e.to_string()),
        // The refusal explains what would be deleted; the status window shows it next to this same action.
        "allow-deletions" => show_status_window(app),
        "pause" => {
            if let Some(s) = &st.svc {
                s.pause();
            }
            Ok(())
        }
        "resume" => {
            if let Some(s) = &st.svc {
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
        // The dialog lives in the status window: open it there (the hash covers a window that isn't loaded yet).
        "cli-install" | "cli-remove" | "cli-repair" => show_status_window_at(app, &id[4..]).map(|_| {
            let _ = app.emit("show-cli", &id[4..]);
        }),
        "quit" => {
            let app = app.clone();
            tauri::async_runtime::spawn(async move {
                if let Some(svc) = app.state::<AppState>().svc.clone() {
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

/// Show the status window; `dialog` (if any) is opened in it when the window is first created.
fn show_status_window_at(app: &AppHandle, dialog: &str) -> Result<(), String> {
    // A menu-bar-only (accessory) app can't bring a window to the front; become a regular app while a window is
    // open, and go back to menu-bar-only when it closes.
    #[cfg(target_os = "macos")]
    let _ = app.set_activation_policy(tauri::ActivationPolicy::Regular);
    if let Some(w) = app.get_webview_window(STATUS_WINDOW) {
        let _ = w.show();
        let _ = w.unminimize();
        return w.set_focus().map_err(|e| e.to_string());
    }
    let page = if dialog.is_empty() { "index.html".to_string() } else { format!("index.html#cli-{dialog}") };
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
    let cfg = st.config.as_ref().ok_or("no config")?;
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
    let Some(cfg) = st.config.clone() else { return };
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
                if let Some(svc) = &st.svc {
                    svc.signed_in();
                }
            }
            Err(e) => tracing::warn!("sign-in failed: {e:#}"),
        }
    });
}

// ---- commands for the status window ----

#[tauri::command]
fn get_status(state: State<'_, AppState>) -> Snapshot {
    let status = state.svc.as_ref().map(Service::status);
    let headline = match (&status, &state.error) {
        (Some(s), _) => s.headline(),
        (None, Some(e)) => first_line(e).to_string(),
        (None, None) => "Not running".into(),
    };
    let mode = status.as_ref().map(Status::mode);
    let mode_text = status.as_ref().map(|s| s.mode().describe(s.poll_interval_secs));
    Snapshot {
        history: state.svc.as_ref().map(|s| s.history(50)).unwrap_or_default(),
        status,
        error: state.error.clone(),
        config_path: state.config_path.display().to_string(),
        headline,
        mode_text,
        mode,
    }
}

#[tauri::command]
fn sync_now(state: State<'_, AppState>, library: Option<String>) -> Result<(), String> {
    let svc = state.svc.as_ref().ok_or("the sync service is not running")?;
    svc.trigger(library.as_deref(), Trigger::Manual).map_err(|e| format!("{e:#}"))
}

#[tauri::command]
fn reconnect(state: State<'_, AppState>, library: Option<String>) -> Result<(), String> {
    let svc = state.svc.as_ref().ok_or("the sync service is not running")?;
    svc.reconnect_push(library.as_deref()).map_err(|e| format!("{e:#}"))
}

/// Re-run the refused libraries with the mass-delete guard overridden (the user saw what would be deleted).
#[tauri::command]
fn allow_deletions(state: State<'_, AppState>, library: Option<String>) -> Result<(), String> {
    let svc = state.svc.clone().ok_or("the sync service is not running")?;
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
    let svc = state.svc.as_ref().ok_or("the sync service is not running")?;
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
