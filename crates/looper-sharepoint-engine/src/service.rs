//! The long-running sync service: one scheduler task per library, shared Graph client (one throttle gate, one
//! pacer), status snapshots, persistent run history, and a broadcast event stream. See `specs/m2-design.md` §3–4.
//! With `sync.push`, one push task per library keeps a live-updates connection open and triggers a run on each change
//! notification; see `specs/m3-design.md`.
//!
//! The service never touches the terminal, windows, signals or the process. Hosts do that.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use looper_sharepoint_auth::{AuthError, TokenProvider};
use looper_sharepoint_core::config::{Config, Library};
use looper_sharepoint_core::state::{StateDb, meta};
use looper_sharepoint_graph::push::PushSocket;
use looper_sharepoint_graph::{GraphClient, GraphError};
use tokio::sync::{Notify, broadcast, mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::lock::HostLock;
use crate::sync::{MassDeleteRefused, RunOptions, sync_library};
use crate::types::{
    Event, Failure, LibraryState, LibraryStatus, Outcome, Progress, PushState, RunRecord, RunResult, Status, Summary,
    Trigger,
};

/// Failed runs back off exponentially from the poll interval, up to this.
pub const MAX_RETRY_DELAY: Duration = Duration::from_secs(30 * 60);
/// While sign-in is required, check this often whether the user has signed in again. This only touches the token
/// endpoint, never SharePoint.
pub const SIGN_IN_RECHECK: Duration = Duration::from_secs(5 * 60);
/// Runs kept in memory for [`Service::history`].
const HISTORY_IN_MEMORY: usize = 100;
/// Live-updates reconnect backoff: doubles from the minimum, capped; reset once a connection has stayed up a while.
const PUSH_BACKOFF_MIN: Duration = Duration::from_secs(2);
const PUSH_BACKOFF_MAX: Duration = Duration::from_secs(5 * 60);
const PUSH_STABLE: Duration = Duration::from_secs(5 * 60);
/// When Graph refuses the push endpoint for a drive (401/403/404), try again this rarely.
const PUSH_UNAVAILABLE_RETRY: Duration = Duration::from_secs(30 * 60);
/// Reconnect with a fresh endpoint this long before the subscription expires.
const PUSH_RENEW_BEFORE: Duration = Duration::from_secs(2 * 60);
/// Local changes start a run once the folder has been quiet this long...
const LOCAL_QUIET: Duration = Duration::from_secs(2);
/// ...or this long after the first change, if changes keep coming.
const LOCAL_MAX_WAIT: Duration = Duration::from_secs(10);
/// Watcher events for paths the engine itself wrote within this window are ignored.
const OWN_WRITE_WINDOW: Duration = Duration::from_secs(3);

pub struct ServiceConfig {
    pub config: Config,
    pub tokens: Arc<dyn TokenProvider>,
    /// Where state DBs, the host lock and history live. Usually [`looper_sharepoint_core::config::data_dir`].
    pub data_dir: PathBuf,
}

/// A running sync service. Cheap to clone; all clones control the same service, also across a [`Service::reload`].
#[derive(Clone)]
pub struct Service {
    shared: Arc<Shared>,
}

struct Shared {
    /// The schedulers for the current config. A reload swaps in a new one.
    current: std::sync::RwLock<Arc<Inner>>,
    data_dir: PathBuf,
    /// The config file this service runs from, if it has one (see [`Service::set_config_file`]).
    config_file: Mutex<Option<PathBuf>>,
    /// One reload at a time.
    reloading: tokio::sync::Mutex<()>,
}

struct Inner {
    cfg: Config,
    graph: GraphClient,
    tokens: Arc<dyn TokenProvider>,
    data_dir: PathBuf,
    events: broadcast::Sender<Event>,
    libs: Vec<Lib>,
    paused: watch::Sender<bool>,
    shutdown: watch::Sender<bool>,
    started_at: DateTime<Utc>,
    history: Mutex<VecDeque<RunRecord>>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
    /// Shared by every `Inner` of this service, so a reload never lets another host in.
    lock: Arc<HostLock>,
}

struct Lib {
    cfg: Library,
    status: Mutex<LibraryStatus>,
    tx: mpsc::UnboundedSender<Request>,
    /// The push task tells the scheduler that the push state changed (so it can switch intervals).
    push_changed: Notify,
    /// A host asked to reconnect live updates now.
    reconnect: Notify,
    /// Local paths (relative, and their parent folders) the engine wrote recently, so the watcher can ignore them.
    own_writes: Mutex<HashMap<String, std::time::Instant>>,
}

struct Request {
    trigger: Trigger,
    opts: RunOptions,
    reply: Option<oneshot::Sender<RunResult>>,
}

impl Service {
    /// Start the service: take the host lock, load history, and spawn one scheduler per library (each runs once at
    /// startup). Must be called from within a tokio runtime.
    pub fn start(sc: ServiceConfig) -> Result<Self> {
        let lock = Arc::new(HostLock::acquire(&sc.data_dir)?);
        let (events, _) = broadcast::channel(1024);
        let inner = Inner::start(sc.config, sc.tokens, sc.data_dir.clone(), events, lock, false, None);
        tracing::info!(libraries = inner.libs.len(), "sync service started");
        Ok(Self {
            shared: Arc::new(Shared {
                current: std::sync::RwLock::new(inner),
                data_dir: sc.data_dir,
                config_file: Mutex::new(None),
                reloading: tokio::sync::Mutex::new(()),
            }),
        })
    }

    fn inner(&self) -> Arc<Inner> {
        self.shared.current.read().unwrap().clone()
    }

    /// Switch to `config` (already parsed and checked): runs in flight finish, then every library is set up again
    /// from the new config, and each syncs once. Libraries keep their state; pause, history and subscribers carry
    /// over. A different account (tenant or app) is refused: that needs a new sign-in, so a restart.
    pub async fn reload(&self, config: Config) -> Result<()> {
        let _one = self.shared.reloading.lock().await;
        let old = self.inner();
        let (a, b) = (&old.cfg.account, &config.account);
        if !a.tenant.eq_ignore_ascii_case(&b.tenant) || !a.client_id.eq_ignore_ascii_case(&b.client_id) {
            bail!("the config now names a different organization or app; quit and start again, then sign in");
        }
        old.shutdown.send_replace(true);
        let tasks: Vec<_> = std::mem::take(&mut *old.tasks.lock().unwrap());
        for t in tasks {
            let _ = t.await;
        }
        let history: Vec<RunRecord> = old.history.lock().unwrap().iter().cloned().collect();
        let paused = *old.paused.borrow();
        let new = Inner::start(config, old.tokens.clone(), self.shared.data_dir.clone(), old.events.clone(), old.lock.clone(), paused, Some(history));
        tracing::info!(libraries = new.libs.len(), "config reloaded");
        *self.shared.current.write().unwrap() = new;
        self.changed();
        Ok(())
    }

    /// Name the config file this service was started from, so [`Service::reload_file`] (and a `reload` request from
    /// the CLI) can read it again. Hosts that keep the config elsewhere don't call this.
    pub fn set_config_file(&self, path: &Path) {
        *self.shared.config_file.lock().unwrap() = Some(path.to_owned());
    }

    pub fn config_file(&self) -> Option<PathBuf> {
        self.shared.config_file.lock().unwrap().clone()
    }

    /// Read the config file again and [`reload`](Service::reload) it. A file that doesn't parse or check keeps the
    /// current config running, and the error says why.
    pub async fn reload_file(&self) -> Result<()> {
        let path = self.config_file().context("this host wasn't started from a config file")?;
        let config = Config::load(&path).with_context(|| format!("not reloaded: {} has a problem; still using the previous config", path.display()))?;
        self.reload(config).await
    }

    /// The current config.
    pub fn config(&self) -> Config {
        self.inner().cfg.clone()
    }

    pub fn data_dir(&self) -> &Path {
        &self.shared.data_dir
    }

    /// A snapshot of everything a host might render.
    pub fn status(&self) -> Status {
        let i = self.inner();
        let stats = i.graph.stats();
        Status {
            account: i.tokens.username(),
            tenant: i.cfg.account.tenant.clone(),
            started_at: i.started_at,
            paused: *i.paused.borrow(),
            throttled_until: i.graph.throttled_until().map(DateTime::<Utc>::from),
            requests: stats.requests,
            throttled_responses: stats.throttled,
            ru_estimate: stats.ru,
            requests_by_kind: stats.by_kind.clone(),
            poll_interval_secs: i.cfg.sync.poll_interval.as_secs(),
            push_safety_interval_secs: i.cfg.sync.push_safety_interval.as_secs(),
            libraries: i.libs.iter().map(|l| l.status.lock().unwrap().clone()).collect(),
        }
    }

    /// Recent runs across all libraries, newest first.
    pub fn history(&self, limit: usize) -> Vec<RunRecord> {
        self.inner().history.lock().unwrap().iter().take(limit).cloned().collect()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.inner().events.subscribe()
    }

    /// Ask for a run now (all libraries if `library` is `None`). Returns immediately. A request made while a run is
    /// in flight is coalesced into one follow-up run.
    pub fn trigger(&self, library: Option<&str>, trigger: Trigger) -> Result<()> {
        let inner = self.inner();
        for lib in select(&inner, library)? {
            let _ = lib.tx.send(Request { trigger, opts: RunOptions::default(), reply: None });
        }
        Ok(())
    }

    /// Run now and wait for the results (all libraries if `library` is `None`).
    pub async fn sync_now(&self, library: Option<&str>, opts: RunOptions) -> Result<Vec<RunResult>> {
        let mut waits = vec![];
        let inner = self.inner();
        for lib in select(&inner, library)? {
            let (tx, rx) = oneshot::channel();
            lib.tx
                .send(Request { trigger: Trigger::Manual, opts: opts.clone(), reply: Some(tx) })
                .map_err(|_| anyhow::anyhow!("service is shutting down"))?;
            waits.push(rx);
        }
        let mut out = vec![];
        for w in waits {
            out.push(w.await.context("service shut down before the run finished")?);
        }
        Ok(out)
    }

    /// Reconnect live updates now (all libraries if `library` is `None`), skipping any backoff wait.
    pub fn reconnect_push(&self, library: Option<&str>) -> Result<()> {
        let inner = self.inner();
        for lib in select(&inner, library)? {
            lib.reconnect.notify_one();
        }
        Ok(())
    }

    /// Stop scheduled runs. Manual runs still happen.
    pub fn pause(&self) {
        self.inner().paused.send_replace(true);
        self.changed();
    }

    pub fn resume(&self) {
        self.inner().paused.send_replace(false);
        self.changed();
    }

    /// Tell the service the user just signed in, so libraries waiting on sign-in run now instead of at the next
    /// recheck.
    pub fn signed_in(&self) {
        for lib in &self.inner().libs {
            if lib.status.lock().unwrap().state == LibraryState::SignInRequired {
                let _ = lib.tx.send(Request { trigger: Trigger::Manual, opts: RunOptions::default(), reply: None });
            }
        }
        self.changed();
    }

    /// Stop the schedulers. A run in flight is allowed to finish (so the state DB and disk stay consistent); hosts
    /// that need to exit immediately can simply drop the runtime instead.
    pub async fn shutdown(&self) {
        let inner = self.inner();
        inner.shutdown.send_replace(true);
        let tasks: Vec<_> = std::mem::take(&mut *inner.tasks.lock().unwrap());
        for t in tasks {
            let _ = t.await;
        }
        tracing::info!("sync service stopped");
    }

    fn changed(&self) {
        let _ = self.inner().events.send(Event::StatusChanged);
    }
}

fn select<'a>(inner: &'a Inner, library: Option<&str>) -> Result<Vec<&'a Lib>> {
    match library {
        None => Ok(inner.libs.iter().collect()),
        Some(name) => match inner.libs.iter().find(|l| l.cfg.local.eq_ignore_ascii_case(name)) {
            Some(l) => Ok(vec![l]),
            None => bail!("no library named {name:?} in config"),
        },
    }
}

impl Inner {
    /// Set up the libraries of `cfg` and spawn their schedulers. `history` carries over from a previous config.
    #[allow(clippy::too_many_arguments)]
    fn start(
        cfg: Config,
        tokens: Arc<dyn TokenProvider>,
        data_dir: PathBuf,
        events: broadcast::Sender<Event>,
        lock: Arc<HostLock>,
        paused: bool,
        history: Option<Vec<RunRecord>>,
    ) -> Arc<Inner> {
        crate::account::configure_client(&cfg);
        let graph = GraphClient::with_rate(tokens.clone(), cfg.sync.max_requests_per_second);
        let mut receivers = vec![];
        let mut libs = vec![];
        let mut runs: Vec<RunRecord> = vec![];
        for lib in &cfg.libraries {
            let (tx, rx) = mpsc::unbounded_channel();
            receivers.push(rx);
            let (status, r) = initial_status(&cfg, &data_dir, lib);
            runs.extend(r);
            libs.push(Lib {
                cfg: lib.clone(),
                status: Mutex::new(status),
                tx,
                push_changed: Notify::new(),
                reconnect: Notify::new(),
                own_writes: Mutex::new(HashMap::new()),
            });
        }
        let history = match history {
            Some(h) => h,
            None => {
                runs.sort_by_key(|r| std::cmp::Reverse(r.started_at));
                runs
            }
        };
        let mut history: VecDeque<RunRecord> = history.into();
        history.truncate(HISTORY_IN_MEMORY);
        let inner = Arc::new(Inner {
            cfg,
            graph,
            tokens,
            data_dir,
            events,
            libs,
            paused: watch::channel(paused).0,
            shutdown: watch::channel(false).0,
            started_at: Utc::now(),
            history: Mutex::new(history),
            tasks: Mutex::new(vec![]),
            lock,
        });
        let mut tasks = vec![];
        for (idx, rx) in receivers.into_iter().enumerate() {
            tasks.push(tokio::spawn(library_loop(inner.clone(), idx, rx)));
            if inner.cfg.sync.push {
                tasks.push(tokio::spawn(push_loop(inner.clone(), idx)));
            }
            if inner.cfg.sync.watch && inner.cfg.uploads(&inner.libs[idx].cfg) {
                tasks.push(tokio::spawn(watch_loop(inner.clone(), idx)));
            }
        }
        tasks.push(tokio::spawn(throttle_watch(inner.clone())));
        *inner.tasks.lock().unwrap() = tasks;
        inner
    }
}

/// Status for a library before its first run in this process, from its state DB's history.
fn initial_status(cfg: &Config, data_dir: &Path, lib: &Library) -> (LibraryStatus, Vec<RunRecord>) {
    let mut st = LibraryStatus {
        name: lib.local.clone(),
        local_path: cfg.local_root(lib).display().to_string(),
        site: lib.site.clone(),
        library: lib.library.clone(),
        state: LibraryState::Idle,
        last_success_at: None,
        last_failure: None,
        next_run_at: None,
        consecutive_failures: 0,
        last_run: None,
        push: if cfg.sync.push { PushState::Connecting { since: Utc::now(), attempt: 0 } } else { PushState::Off },
        last_push_at: None,
        watching: false,
        uploads: cfg.uploads(lib),
    };
    let Some(db) = open_db(data_dir, &lib.local) else {
        return (st, vec![]);
    };
    let parse = |j: String| serde_json::from_str::<RunRecord>(&j).ok();
    let runs: Vec<RunRecord> = db.recent_runs(HISTORY_IN_MEMORY).unwrap_or_default().into_iter().filter_map(parse).collect();
    st.last_run = runs.first().cloned();
    st.last_success_at = db
        .last_run_with(&["ok", "problems"])
        .ok()
        .flatten()
        .and_then(parse)
        .map(|r| r.finished_at)
        .or_else(|| {
            // M1 runs predate the history table.
            let t = db.get_meta(meta::LAST_SYNC_AT).ok().flatten()?;
            DateTime::parse_from_rfc3339(&t).ok().map(|t| t.with_timezone(&Utc))
        });
    st.last_failure = db.last_run_with(&["failed", "refused"]).ok().flatten().and_then(parse).map(|r| Failure {
        at: r.finished_at,
        error: r.error.unwrap_or_default(),
    });
    (st, runs)
}

fn open_db(data_dir: &Path, local: &str) -> Option<StateDb> {
    let idx = crate::index::load(data_dir).ok()?;
    let entry = idx.get(&local.to_lowercase())?;
    StateDb::open(&entry.db_path).ok()
}

async fn library_loop(inner: Arc<Inner>, idx: usize, mut rx: mpsc::UnboundedReceiver<Request>) {
    let lib = &inner.libs[idx];
    let poll = inner.cfg.sync.poll_interval;
    let safety = inner.cfg.sync.push_safety_interval;
    let mut last_finished: Option<Instant> = None;
    let mut shutdown = inner.shutdown.subscribe();
    let mut paused = inner.paused.subscribe();
    let mut backlog: VecDeque<Request> = VecDeque::new();
    // Run once at startup.
    let mut next: Option<Instant> = Some(Instant::now());
    let mut first = true;
    loop {
        if *shutdown.borrow() {
            return;
        }
        let poll_at = next.filter(|_| !*paused.borrow());
        set_next_run(&inner, idx, poll_at);

        let req = if let Some(r) = backlog.pop_front() {
            Some(r)
        } else {
            tokio::select! {
                _ = shutdown.changed() => return,
                _ = paused.changed() => continue,
                r = rx.recv() => match r {
                    Some(r) => Some(r),
                    None => return,
                },
                _ = sleep_until(poll_at) => None,
                _ = lib.push_changed.notified() => {
                    // Live updates dropped: fall back to polling now, not at the end of a long safety interval.
                    if !lib.status.lock().unwrap().push.is_live()
                        && let Some(n) = next
                        && n > Instant::now() + poll
                    {
                        let due = last_finished.map_or_else(Instant::now, |f| f + jittered(poll));
                        next = Some(due.max(Instant::now()));
                    }
                    continue;
                }
            }
        };

        let (trigger, opts, mut replies) = match req {
            Some(r) => {
                // Coalesce everything else queued with the same options into this run.
                let mut replies: Vec<_> = r.reply.into_iter().collect();
                while let Ok(more) = rx.try_recv() {
                    if more.opts == r.opts {
                        replies.extend(more.reply);
                    } else {
                        backlog.push_back(more);
                    }
                }
                (r.trigger, r.opts, replies)
            }
            None if first => (Trigger::Startup, RunOptions::default(), vec![]),
            None if inner.libs[idx].status.lock().unwrap().consecutive_failures > 0 => {
                (Trigger::Retry, RunOptions::default(), vec![])
            }
            None => (Trigger::Poll, RunOptions::default(), vec![]),
        };
        first = false;

        let result = run_once(&inner, idx, trigger, &opts).await;
        if !opts.dry_run {
            last_finished = Some(Instant::now());
            let (state, failures, live) = {
                let st = lib.status.lock().unwrap();
                (st.state.clone(), st.consecutive_failures, st.push.is_live())
            };
            let mut delay = next_delay(&state, poll, live.then_some(safety), failures);
            if let (Some(d), Some(until)) = (delay, inner.graph.throttled_until()) {
                let wait = until.duration_since(std::time::SystemTime::now()).unwrap_or_default();
                delay = Some(d.max(wait + jitter(Duration::from_secs(5))));
            }
            next = delay.map(|d| Instant::now() + d);
        }
        for r in replies.drain(..) {
            let _ = r.send(result.clone());
        }
    }
}

/// When to run next after a run that left the library in `state`. `None` means only on request. `live` is the safety
/// interval when live updates are connected (changes then arrive as notifications instead).
fn next_delay(state: &LibraryState, poll: Duration, live: Option<Duration>, consecutive_failures: u32) -> Option<Duration> {
    match state {
        LibraryState::Refused => None,
        LibraryState::SignInRequired => Some(SIGN_IN_RECHECK),
        LibraryState::Error => {
            let factor = 1u32 << consecutive_failures.min(8);
            Some((poll * factor).min(MAX_RETRY_DELAY))
        }
        _ => Some(jittered(live.unwrap_or(poll))),
    }
}

fn note_own_write(lib: &Lib, paths: &[&String]) {
    let now = std::time::Instant::now();
    let mut w = lib.own_writes.lock().unwrap();
    w.retain(|_, t| now.duration_since(*t) < OWN_WRITE_WINDOW);
    for p in paths {
        let mut a = p.as_str();
        w.insert(a.to_string(), now);
        while let Some((parent, _)) = a.rsplit_once('/') {
            w.insert(parent.to_string(), now);
            a = parent;
        }
    }
}

/// Is a watcher event about a path the user (or another program) changed, as opposed to us or ignored files?
fn user_change(lib: &Lib, ignore: &looper_sharepoint_core::ignore::Ignore, root: &Path, ev: &notify::Event) -> bool {
    use notify::EventKind;
    if matches!(ev.kind, EventKind::Access(_)) {
        return false;
    }
    let now = std::time::Instant::now();
    let own = lib.own_writes.lock().unwrap();
    ev.paths.iter().any(|p| {
        let Ok(rel) = p.strip_prefix(root) else { return false };
        let rel = rel.to_string_lossy().replace('\\', "/");
        if rel.is_empty() || ignore.is_ignored(&rel) {
            return false;
        }
        own.get(&rel).is_none_or(|t| now.duration_since(*t) >= OWN_WRITE_WINDOW)
    })
}

/// Watch a two-way library's folder and start a run shortly after local changes settle. See `specs/m4-design.md` §6.
async fn watch_loop(inner: Arc<Inner>, idx: usize) {
    use notify::{RecursiveMode, Watcher};
    let lib = &inner.libs[idx];
    let name = lib.cfg.local.clone();
    let mut shutdown = inner.shutdown.subscribe();
    let mut events = inner.events.subscribe();
    let ignore = match looper_sharepoint_core::ignore::Ignore::new(&inner.cfg.sync.ignore) {
        Ok(i) => i,
        Err(e) => {
            tracing::warn!(library = name, "not watching: {e}");
            return;
        }
    };
    let set_watching = |on: bool| {
        let mut st = lib.status.lock().unwrap();
        if st.watching != on {
            st.watching = on;
            drop(st);
            let _ = inner.events.send(Event::StatusChanged);
        }
    };
    loop {
        if *shutdown.borrow() {
            return;
        }
        let root = inner.cfg.local_root(&lib.cfg);
        // FSEvents reports canonical paths (e.g. /private/tmp for /tmp).
        let root = std::fs::canonicalize(&root).unwrap_or(root);
        let (tx, mut rx) = mpsc::unbounded_channel::<notify::Event>();
        let watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if let Ok(ev) = res {
                let _ = tx.send(ev);
            }
        })
        .and_then(|mut w| w.watch(&root, RecursiveMode::Recursive).map(|_| w));
        let _watcher = match watcher {
            Ok(w) => w,
            Err(e) => {
                // Usually: the folder doesn't exist yet (first sync pending). Try again after the next run.
                tracing::debug!(library = name, "not watching yet: {e}");
                set_watching(false);
                tokio::select! {
                    _ = shutdown.changed() => return,
                    _ = async { while !matches!(events.recv().await, Ok(Event::RunFinished { .. }) | Err(_)) {} } => {}
                    _ = tokio::time::sleep(Duration::from_secs(60)) => {}
                }
                continue;
            }
        };
        tracing::info!(library = name, root = %root.display(), "watching for local changes");
        set_watching(true);
        let mut first: Option<Instant> = None;
        let mut quiet_until = Instant::now();
        loop {
            let due = first.map(|f| quiet_until.min(f + LOCAL_MAX_WAIT));
            tokio::select! {
                _ = shutdown.changed() => return,
                ev = rx.recv() => match ev {
                    Some(ev) => {
                        if user_change(lib, &ignore, &root, &ev) {
                            first.get_or_insert_with(Instant::now);
                            quiet_until = Instant::now() + LOCAL_QUIET;
                        }
                    }
                    None => break, // watcher died; re-create it
                },
                _ = sleep_until(due) => {
                    first = None;
                    if !*inner.paused.borrow() {
                        tracing::debug!(library = name, "local changes settled; syncing");
                        let _ = lib.tx.send(Request { trigger: Trigger::Local, opts: RunOptions::default(), reply: None });
                    }
                }
            }
        }
        set_watching(false);
    }
}

/// Why a library can't use live updates right now (no runs would happen anyway), if so.
fn push_blocked(inner: &Inner, idx: usize) -> Option<&'static str> {
    if *inner.paused.borrow() {
        return Some("paused");
    }
    match inner.libs[idx].status.lock().unwrap().state {
        LibraryState::SignInRequired => Some("sign-in required"),
        LibraryState::Refused => Some("waiting for a manual sync (mass-delete guard)"),
        _ => None,
    }
}

fn set_push(inner: &Inner, idx: usize, push: PushState) {
    let lib = &inner.libs[idx];
    let mut st = lib.status.lock().unwrap();
    if st.push == push {
        return;
    }
    let was_live = st.push.is_live();
    st.push = push;
    let live = st.push.is_live();
    drop(st);
    if was_live != live {
        lib.push_changed.notify_one();
    }
    let _ = inner.events.send(Event::StatusChanged);
}

fn wall(d: Duration) -> DateTime<Utc> {
    Utc::now() + chrono::Duration::from_std(d).unwrap_or_default()
}

fn push_backoff(attempt: u32) -> Duration {
    let d = (PUSH_BACKOFF_MIN * (1u32 << attempt.saturating_sub(1).min(10))).min(PUSH_BACKOFF_MAX);
    // ±20%
    let ms = d.as_millis() as u64;
    Duration::from_millis(ms - ms / 5 + fastrand::u64(0..=2 * (ms / 5)))
}

/// Test hook: while the file named by `LOOPER_SHAREPOINT_TEST_PUSH_OUTAGE` exists, live updates behave as if the
/// network to the push service were down (connects fail, an open socket is lost). Used by the live tests to exercise
/// the polling fallback without touching the machine's network.
fn simulated_outage() -> bool {
    std::env::var_os("LOOPER_SHAREPOINT_TEST_PUSH_OUTAGE").is_some_and(|p| Path::new(&p).exists())
}

enum Connect {
    Ok(Box<PushSocket>, Option<DateTime<Utc>>),
    /// Graph refused the endpoint for this drive; retry rarely.
    Unavailable(String),
    Failed(String),
}

/// Get a fresh endpoint and connect. `drive` caches the resolved drive id.
async fn push_connect(inner: &Inner, idx: usize, drive: &mut Option<String>) -> Connect {
    let lib = &inner.libs[idx].cfg;
    if simulated_outage() {
        return Connect::Failed("simulated outage (test hook)".into());
    }
    if drive.is_none() {
        let cached = crate::index::cached(&inner.data_dir, lib);
        match async { match cached { Some(r) => Ok(r), None => crate::sync::resolve(&inner.graph, lib).await } }.await {
            Ok(r) => *drive = Some(r.drive_id),
            Err(e) => return Connect::Failed(format!("{e:#}")),
        }
    }
    let sub = match inner.graph.socket_io_endpoint(drive.as_deref().unwrap_or_default()).await {
        Ok(s) => s,
        Err(GraphError::NotFound(m)) => return Connect::Unavailable(format!("not offered for this library ({m})")),
        Err(GraphError::Http { status: s @ (401 | 403 | 404), code, message }) => {
            return Connect::Unavailable(format!("refused by SharePoint: {s} {code}: {message}"));
        }
        Err(e) => return Connect::Failed(e.to_string()),
    };
    let expires = sub.expiration_date_time.as_deref().and_then(|t| DateTime::parse_from_rfc3339(t).ok()).map(|t| t.with_timezone(&Utc));
    match PushSocket::connect(&sub.notification_url).await {
        Ok(sock) => Connect::Ok(Box::new(sock), expires),
        Err(e) => Connect::Failed(e.to_string()),
    }
}

/// Keep a library's live-updates connection open while it can sync, and fall back to polling (via
/// [`set_push`] → the scheduler) whenever it isn't. See `specs/m3-design.md` §3.
async fn push_loop(inner: Arc<Inner>, idx: usize) {
    let lib = &inner.libs[idx];
    let name = lib.cfg.local.clone();
    let mut shutdown = inner.shutdown.subscribe();
    let mut events = inner.events.subscribe();
    let mut drive: Option<String> = None;
    let mut attempt = 0u32;
    // When live updates were lost (or first tried); cleared while live.
    let mut down_since = Utc::now();
    // Swapping an expiring subscription for a fresh one: stay "live" through the reconnect, so the scheduler doesn't
    // fall back to polling for a second.
    let mut renewing = false;
    let trigger = || {
        let _ = lib.tx.send(Request { trigger: Trigger::Push, opts: RunOptions::default(), reply: None });
    };

    loop {
        if *shutdown.borrow() {
            return;
        }
        // Wait while the library can't sync anyway.
        if let Some(reason) = push_blocked(&inner, idx) {
            set_push(&inner, idx, PushState::Down { since: down_since, reason: reason.into(), retry_at: None });
            tokio::select! {
                _ = shutdown.changed() => return,
                _ = events.recv() => {}
                _ = lib.reconnect.notified() => {}
            }
            continue;
        }
        // While throttled, don't add requests; say so.
        if let Some(until) = inner.graph.throttled_until() {
            let wait = until.duration_since(std::time::SystemTime::now()).unwrap_or_default();
            if !wait.is_zero() {
                let reason = "SharePoint is throttling requests".to_string();
                set_push(&inner, idx, PushState::Down { since: down_since, reason, retry_at: Some(until.into()) });
                tokio::select! {
                    _ = shutdown.changed() => return,
                    _ = tokio::time::sleep(wait) => {}
                }
                continue;
            }
        }

        if !std::mem::take(&mut renewing) {
            set_push(&inner, idx, PushState::Connecting { since: down_since, attempt });
        }
        let (mut sock, expires) = match push_connect(&inner, idx, &mut drive).await {
            Connect::Ok(sock, expires) => (sock, expires),
            Connect::Unavailable(reason) => {
                tracing::warn!(library = name, "live updates unavailable: {reason}");
                let retry_at = wall(PUSH_UNAVAILABLE_RETRY);
                set_push(&inner, idx, PushState::Unavailable { since: down_since, reason, retry_at });
                attempt = 0;
                tokio::select! {
                    _ = shutdown.changed() => return,
                    _ = tokio::time::sleep(PUSH_UNAVAILABLE_RETRY) => {}
                    _ = lib.reconnect.notified() => {}
                }
                continue;
            }
            Connect::Failed(reason) => {
                attempt += 1;
                let delay = push_backoff(attempt);
                tracing::info!(library = name, attempt, retry_in = ?delay, "live updates: connect failed: {reason}");
                set_push(&inner, idx, PushState::Down { since: down_since, reason, retry_at: Some(wall(delay)) });
                tokio::select! {
                    _ = shutdown.changed() => return,
                    _ = tokio::time::sleep(delay) => {}
                    _ = lib.reconnect.notified() => attempt = 0,
                }
                continue;
            }
        };

        tracing::info!(library = name, expires = ?expires, "live updates connected");
        let connected = Instant::now();
        if !lib.status.lock().unwrap().push.is_live() {
            set_push(&inner, idx, PushState::Live { since: Utc::now() });
        }
        // Notifications sent while we weren't connected (even briefly, for a renewal) are lost: catch up.
        trigger();
        let renew_at = expires.map(|e| {
            let left = (e - Utc::now()).to_std().unwrap_or_default().saturating_sub(PUSH_RENEW_BEFORE);
            connected + left.max(Duration::from_secs(30))
        });

        enum Why {
            Shutdown,
            Renew,
            Reconnect,
            Blocked,
            Lost(String),
        }
        let mut outage_check = tokio::time::interval(Duration::from_secs(1));
        let outage_hook = std::env::var_os("LOOPER_SHAREPOINT_TEST_PUSH_OUTAGE").is_some();
        let why = loop {
            tokio::select! {
                _ = outage_check.tick(), if outage_hook => if simulated_outage() { break Why::Lost("simulated outage (test hook)".into()) },
                _ = shutdown.changed() => break Why::Shutdown,
                _ = sleep_until(renew_at) => break Why::Renew,
                _ = lib.reconnect.notified() => break Why::Reconnect,
                _ = events.recv() => if push_blocked(&inner, idx).is_some() { break Why::Blocked },
                n = sock.next() => match n {
                    Ok(n) => {
                        tracing::debug!(library = name, ?n, "change notification");
                        lib.status.lock().unwrap().last_push_at = Some(Utc::now());
                        trigger();
                    }
                    Err(e) => break Why::Lost(e.to_string()),
                },
            }
        };
        let _ = tokio::time::timeout(Duration::from_secs(2), sock.close()).await;
        if connected.elapsed() >= PUSH_STABLE {
            attempt = 0;
        }
        down_since = Utc::now();
        match why {
            Why::Shutdown => return,
            // A fresh endpoint replaces the expiring one; no fallback needed in between.
            Why::Renew => {
                tracing::info!(library = name, "live updates: renewing subscription");
                renewing = true;
            }
            Why::Reconnect => attempt = 0,
            Why::Blocked => {}
            Why::Lost(reason) => {
                attempt += 1;
                let delay = push_backoff(attempt);
                tracing::warn!(library = name, retry_in = ?delay, "live updates lost: {reason}");
                set_push(&inner, idx, PushState::Down { since: down_since, reason, retry_at: Some(wall(delay)) });
                tokio::select! {
                    _ = shutdown.changed() => return,
                    _ = tokio::time::sleep(delay) => {}
                    _ = lib.reconnect.notified() => attempt = 0,
                }
            }
        }
    }
}

/// `d` ± 10%, so a fleet of devices never polls in lockstep.
fn jittered(d: Duration) -> Duration {
    let ms = d.as_millis() as u64;
    let spread = ms / 10;
    Duration::from_millis(ms - spread + fastrand::u64(0..=2 * spread))
}

fn jitter(max: Duration) -> Duration {
    Duration::from_millis(fastrand::u64(0..=max.as_millis() as u64))
}

async fn sleep_until(t: Option<Instant>) {
    match t {
        Some(t) => tokio::time::sleep_until(t).await,
        None => std::future::pending().await,
    }
}

fn set_next_run(inner: &Inner, idx: usize, at: Option<Instant>) {
    let wall = at.map(|t| {
        let d = t.saturating_duration_since(Instant::now());
        Utc::now() + chrono::Duration::from_std(d).unwrap_or_default()
    });
    let mut st = inner.libs[idx].status.lock().unwrap();
    // Only signal when it actually moved, to keep subscribers quiet.
    let moved = match (st.next_run_at, wall) {
        (Some(a), Some(b)) => (a - b).num_seconds().abs() > 1,
        (a, b) => a.is_some() != b.is_some(),
    };
    st.next_run_at = wall;
    drop(st);
    if moved {
        let _ = inner.events.send(Event::StatusChanged);
    }
}

async fn run_once(inner: &Arc<Inner>, idx: usize, trigger: Trigger, opts: &RunOptions) -> RunResult {
    let lib = &inner.libs[idx];
    let name = lib.cfg.local.clone();
    let started_at = Utc::now();
    let before = {
        let mut st = lib.status.lock().unwrap();
        let before = st.state.clone();
        st.state = LibraryState::Syncing { trigger, started_at, done: 0, total: 0 };
        st.next_run_at = None;
        before
    };
    tracing::info!(library = name, trigger = trigger.as_str(), dry_run = opts.dry_run, "sync run starting");
    let _ = inner.events.send(Event::RunStarted { library: name.clone(), trigger });
    let _ = inner.events.send(Event::StatusChanged);

    let progress = |p: Progress| {
        if let Progress::Downloading { done, total, .. } | Progress::Uploading { done, total, .. } = &p
            && let LibraryState::Syncing { done: d, total: t, .. } = &mut lib.status.lock().unwrap().state
        {
            (*d, *t) = (*done, *total);
        }
        // Remember what we write locally, so the watcher doesn't treat it as a user change.
        match &p {
            Progress::Downloaded { path } | Progress::Deleted { path } => note_own_write(lib, &[path]),
            Progress::Moved { from, to } => note_own_write(lib, &[from, to]),
            Progress::ConflictCopy { path, copy } => note_own_write(lib, &[path, copy]),
            _ => {}
        }
        let _ = inner.events.send(Event::Progress { library: name.clone(), progress: p });
    };
    let result = sync_library(&inner.graph, &inner.cfg, &inner.data_dir, &lib.cfg, opts, &progress).await;

    let finished_at = Utc::now();
    let (outcome, error, sign_in, report) = match result {
        Ok(r) => (if r.has_problems() { Outcome::Problems } else { Outcome::Ok }, None, false, Some(r)),
        Err(e) if e.downcast_ref::<MassDeleteRefused>().is_some() => (Outcome::Refused, Some(e.to_string()), false, None),
        Err(e) => (Outcome::Failed, Some(format!("{e:#}")), needs_sign_in(&e), None),
    };
    let record = RunRecord {
        library: name.clone(),
        trigger,
        started_at,
        finished_at,
        outcome,
        error,
        summary: report.as_ref().map(Summary::from_report),
        issues: report
            .as_ref()
            .map(|r| r.conflicts.iter().chain(&r.errors).take(RunRecord::MAX_ISSUES).cloned().collect())
            .unwrap_or_default(),
    };
    match &record.error {
        Some(e) if sign_in => tracing::info!(library = name, reason = e, "sign-in required; not syncing"),
        Some(e) => tracing::warn!(library = name, outcome = outcome.as_str(), error = e, "sync run did not complete"),
        None => tracing::info!(library = name, outcome = outcome.as_str(), summary = record.describe(), "sync run finished"),
    }

    if opts.dry_run {
        lib.status.lock().unwrap().state = before;
    } else if sign_in {
        // Not a failure: the state already says "sign-in required", and recording every recheck would bury real
        // failures in the history.
        lib.status.lock().unwrap().state = LibraryState::SignInRequired;
    } else {
        persist(inner, &name, &record);
        let mut st = lib.status.lock().unwrap();
        st.state = match outcome {
            Outcome::Ok | Outcome::Problems => LibraryState::Idle,
            Outcome::Refused => LibraryState::Refused,
            Outcome::Failed => LibraryState::Error,
        };
        if outcome.is_success() {
            st.last_success_at = Some(finished_at);
            st.consecutive_failures = 0;
        } else {
            st.last_failure = Some(Failure { at: finished_at, error: record.error.clone().unwrap_or_default() });
            st.consecutive_failures += 1;
        }
        st.last_run = Some(record.clone());
        drop(st);
        let mut h = inner.history.lock().unwrap();
        h.push_front(record.clone());
        h.truncate(HISTORY_IN_MEMORY);
    }
    let _ = inner.events.send(Event::RunFinished { record: record.clone() });
    let _ = inner.events.send(Event::StatusChanged);
    RunResult { record, report }
}

fn persist(inner: &Inner, local: &str, record: &RunRecord) {
    let Some(db) = open_db(&inner.data_dir, local) else {
        return; // never resolved (e.g. the first run failed early); history stays in memory
    };
    let json = serde_json::to_string(record).unwrap_or_default();
    if let Err(e) = db.add_run(&record.started_at.to_rfc3339(), record.outcome.as_str(), &json) {
        tracing::warn!(library = local, "recording run history: {e}");
    }
}

/// Is this failure "the user has to sign in again" (rather than e.g. the network being down)?
pub fn needs_sign_in(e: &anyhow::Error) -> bool {
    e.chain().any(|c| {
        c.downcast_ref::<AuthError>().is_some_and(AuthError::needs_sign_in)
            || matches!(c.downcast_ref::<GraphError>(), Some(GraphError::Auth(a)) if a.needs_sign_in())
    })
}

async fn throttle_watch(inner: Arc<Inner>) {
    let mut rx = inner.graph.watch_throttle();
    let mut shutdown = inner.shutdown.subscribe();
    loop {
        tokio::select! {
            _ = shutdown.changed() => return,
            r = rx.changed() => if r.is_err() { return },
        }
        let until = *rx.borrow_and_update();
        if let Some(until) = until {
            let _ = inner.events.send(Event::Throttled { until: until.into() });
            let _ = inner.events.send(Event::StatusChanged);
            // Tell subscribers again when the pause is over (unless it gets extended first).
            let wait = until.duration_since(std::time::SystemTime::now()).unwrap_or_default();
            tokio::select! {
                _ = shutdown.changed() => return,
                _ = tokio::time::sleep(wait) => { let _ = inner.events.send(Event::StatusChanged); }
                r = rx.changed() => if r.is_err() { return },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jitter_stays_within_ten_percent() {
        for _ in 0..1000 {
            let d = jittered(Duration::from_secs(45));
            assert!(d >= Duration::from_millis(40_500) && d <= Duration::from_millis(49_500), "{d:?}");
        }
    }

    #[test]
    fn failures_back_off_and_cap() {
        let i = Duration::from_secs(45);
        assert_eq!(next_delay(&LibraryState::Error, i, None, 1), Some(Duration::from_secs(90)));
        assert_eq!(next_delay(&LibraryState::Error, i, None, 3), Some(Duration::from_secs(360)));
        assert_eq!(next_delay(&LibraryState::Error, i, None, 30), Some(MAX_RETRY_DELAY));
        assert_eq!(next_delay(&LibraryState::Refused, i, None, 1), None);
        assert_eq!(next_delay(&LibraryState::SignInRequired, i, None, 4), Some(SIGN_IN_RECHECK));
        let ok = next_delay(&LibraryState::Idle, i, None, 0).unwrap();
        assert!(ok >= Duration::from_millis(40_500) && ok <= Duration::from_millis(49_500));
    }

    #[test]
    fn live_updates_slow_the_schedule_but_not_retries() {
        let (poll, safety) = (Duration::from_secs(45), Duration::from_secs(900));
        let d = next_delay(&LibraryState::Idle, poll, Some(safety), 0).unwrap();
        assert!(d >= Duration::from_secs(810) && d <= Duration::from_secs(990), "{d:?}");
        // A failing library retries on the poll-based backoff even while live.
        assert_eq!(next_delay(&LibraryState::Error, poll, Some(safety), 1), Some(Duration::from_secs(90)));
    }

    #[test]
    fn push_backoff_grows_and_caps() {
        for _ in 0..200 {
            let b1 = push_backoff(1);
            assert!(b1 >= Duration::from_millis(1600) && b1 <= Duration::from_millis(2400), "{b1:?}");
            let b4 = push_backoff(4);
            assert!(b4 >= Duration::from_millis(12_800) && b4 <= Duration::from_millis(19_200), "{b4:?}");
            assert!(push_backoff(30) <= PUSH_BACKOFF_MAX + PUSH_BACKOFF_MAX / 5);
        }
    }

    #[test]
    fn sign_in_errors_are_recognized() {
        let e = anyhow::Error::from(GraphError::Auth(AuthError::ReauthRequired("expired".into()))).context("resolving site");
        assert!(needs_sign_in(&e));
        let e = anyhow::Error::from(AuthError::NotSignedIn);
        assert!(needs_sign_in(&e));
        let e = anyhow::Error::from(GraphError::Retryable("timeout".into()));
        assert!(!needs_sign_in(&e));
    }
}
