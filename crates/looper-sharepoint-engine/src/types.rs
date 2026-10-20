//! Plain data types the service exposes. All are serde types, so hosts can pass them over IPC or into a webview as
//! JSON unchanged.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::sync::{Issue, Report};

/// What started a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Trigger {
    Startup,
    Poll,
    /// A user or host asked for it (`trigger`, `sync`, "Sync now").
    Manual,
    /// A change notification (M3).
    Push,
    /// A retry after a failed run.
    Retry,
    /// Files changed in the library folder (the watcher).
    Local,
}

impl Trigger {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Startup => "startup",
            Self::Poll => "poll",
            Self::Manual => "manual",
            Self::Push => "push",
            Self::Retry => "retry",
            Self::Local => "local",
        }
    }
}

/// Emitted during a run, in order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Progress {
    /// Remote changes fetched and the plan passed the mass-delete guard.
    Planned { changes: usize, deletes: usize },
    Downloading { done: usize, total: usize, bytes_done: u64, bytes_total: u64 },
    Downloaded { path: String },
    Deleted { path: String },
    Moved { from: String, to: String },
    Uploading { done: usize, total: usize, bytes_done: u64, bytes_total: u64 },
    /// A local file was uploaded (new or in place).
    Uploaded { path: String },
    /// A local rename/move was applied in SharePoint.
    MovedRemote { from: String, to: String },
    /// A local delete was applied in SharePoint (recycle bin).
    DeletedRemote { path: String },
    /// Both sides changed: the local version was kept as `copy` (uploaded as a new file).
    ConflictCopy { path: String, copy: String },
    Conflict { path: String, reason: String },
    Error { path: String, reason: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// Everything applied.
    Ok,
    /// Finished, but some items ended in conflict or error.
    Problems,
    /// The run itself failed (network, auth, config, ...).
    Failed,
    /// The mass-delete guard stopped it before anything changed.
    Refused,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Problems => "problems",
            Self::Failed => "failed",
            Self::Refused => "refused",
        }
    }

    pub fn is_success(self) -> bool {
        matches!(self, Self::Ok | Self::Problems)
    }
}

/// Counts from a run (lists are summarized; see [`Report`] for the full detail).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Summary {
    pub changes: usize,
    pub downloaded: usize,
    pub bytes_downloaded: u64,
    pub adopted: usize,
    pub moved: usize,
    pub deleted: usize,
    pub conflicts: usize,
    pub errors: usize,
    pub skipped: usize,
    pub orphans: usize,
    pub local_edits_kept: usize,
    pub uploaded: usize,
    pub bytes_uploaded: u64,
    pub created_remote: usize,
    pub moved_remote: usize,
    pub deleted_remote: usize,
    pub conflict_copies: usize,
    pub full_enumeration: bool,
    pub resynced: bool,
}

impl Summary {
    pub fn from_report(r: &Report) -> Self {
        Self {
            changes: r.delta_items,
            downloaded: r.downloaded,
            bytes_downloaded: r.bytes_downloaded,
            adopted: r.adopted,
            moved: r.moved,
            deleted: r.deleted_local.len(),
            conflicts: r.conflicts.len(),
            errors: r.errors.len(),
            skipped: r.skipped.len(),
            orphans: r.orphans.len(),
            local_edits_kept: r.local_edits_kept.len(),
            uploaded: r.uploaded,
            bytes_uploaded: r.bytes_uploaded,
            created_remote: r.created_remote,
            moved_remote: r.moved_remote,
            deleted_remote: r.deleted_remote.len(),
            conflict_copies: r.conflict_copies.len(),
            full_enumeration: r.full_enumeration,
            resynced: r.resynced,
        }
    }

    /// One line for logs, the TUI and the tray, e.g. "2 downloaded, 1 moved".
    pub fn describe(&self) -> String {
        let mut parts = vec![];
        let mut add = |n: usize, one: &str, many: &str| {
            if n > 0 {
                parts.push(format!("{n} {}", if n == 1 { one } else { many }));
            }
        };
        add(self.downloaded, "downloaded", "downloaded");
        add(self.uploaded, "uploaded", "uploaded");
        add(self.created_remote, "folder created in SharePoint", "folders created in SharePoint");
        add(self.moved, "moved", "moved");
        add(self.moved_remote, "moved in SharePoint", "moved in SharePoint");
        add(self.deleted, "deleted", "deleted");
        add(self.deleted_remote, "deleted in SharePoint", "deleted in SharePoint");
        add(self.conflict_copies, "conflict copy", "conflict copies");
        add(self.adopted, "adopted", "adopted");
        add(self.conflicts, "conflict", "conflicts");
        add(self.errors, "error", "errors");
        if parts.is_empty() {
            "no changes".into()
        } else {
            parts.join(", ")
        }
    }
}

/// One entry in a library's run history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunRecord {
    pub library: String,
    pub trigger: Trigger,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    pub outcome: Outcome,
    /// Set when `outcome` is `failed` or `refused`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<Summary>,
    /// Conflicts and errors, at most [`RunRecord::MAX_ISSUES`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub issues: Vec<Issue>,
}

impl RunRecord {
    pub const MAX_ISSUES: usize = 20;

    pub fn duration_ms(&self) -> i64 {
        (self.finished_at - self.started_at).num_milliseconds()
    }

    /// One line describing the result.
    pub fn describe(&self) -> String {
        match (&self.error, &self.summary) {
            (Some(e), _) => e.clone(),
            (None, Some(s)) => s.describe(),
            (None, None) => self.outcome.as_str().into(),
        }
    }
}

/// What a library is doing right now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum LibraryState {
    /// Up to date as of the last run (which may have had item-level problems; see `last_run`).
    Idle,
    Syncing {
        trigger: Trigger,
        started_at: DateTime<Utc>,
        /// Files downloaded / to download in this run, once known.
        done: usize,
        total: usize,
    },
    /// The last run failed; it will be retried at `next_run_at`.
    Error,
    /// Sign-in expired or was revoked. Runs resume after the user signs in again.
    SignInRequired,
    /// The mass-delete guard refused the last run; it needs a manual run with the override.
    Refused,
}

/// Live updates (Graph socket.io push) for one library. See `specs/m3-design.md` §2.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum PushState {
    /// `sync.push = false`: polling by choice.
    Off,
    Connecting { since: DateTime<Utc>, attempt: u32 },
    /// Connected: changes are announced within ~30 s; scheduled checks slow to `push_safety_interval`.
    Live { since: DateTime<Utc> },
    /// Enabled but not connected; the library polls at `poll_interval`. `retry_at` is `None` while the library
    /// can't sync anyway (paused, sign-in required, refused).
    Down { since: DateTime<Utc>, reason: String, retry_at: Option<DateTime<Utc>> },
    /// Graph refused the push endpoint (401/403/404) for this drive; retried rarely.
    Unavailable { since: DateTime<Utc>, reason: String, retry_at: DateTime<Utc> },
}

impl PushState {
    pub fn is_live(&self) -> bool {
        matches!(self, PushState::Live { .. })
    }
}

/// How the service as a whole learns about remote changes, for a prominent status line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum Mode {
    /// Every library is live.
    Live,
    /// Some libraries are live, the rest are polling.
    PartlyLive { live: usize, total: usize },
    /// Live updates are starting.
    Connecting,
    /// Live updates are enabled but down; polling. `since` and `reason` come from the first library that is down.
    Polling { since: DateTime<Utc>, reason: String },
    /// Live updates are off in config (or no libraries); polling.
    Off,
}

impl Mode {
    pub fn is_live(&self) -> bool {
        matches!(self, Mode::Live)
    }

    /// A short line, e.g. "Live updates on" or "Live updates down · checking every 45s". Hosts add times and reasons.
    pub fn describe(&self, poll_interval_secs: u64) -> String {
        let every = format!("checking every {}", human_secs(poll_interval_secs));
        match self {
            Mode::Live => "Live updates on".into(),
            Mode::PartlyLive { live, total } => format!("Live updates on for {live} of {total} libraries · others {every}"),
            Mode::Connecting => "Connecting live updates…".into(),
            Mode::Polling { .. } => format!("Live updates down · {every}"),
            Mode::Off => format!("Live updates off · {every}"),
        }
    }
}

fn human_secs(s: u64) -> String {
    match s {
        s if s % 3600 == 0 => format!("{}h", s / 3600),
        s if s % 60 == 0 => format!("{}m", s / 60),
        s => format!("{s}s"),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Failure {
    pub at: DateTime<Utc>,
    pub error: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LibraryStatus {
    /// The library's `local` name from config.
    pub name: String,
    pub local_path: String,
    pub site: String,
    pub library: String,
    #[serde(flatten)]
    pub state: LibraryState,
    pub last_success_at: Option<DateTime<Utc>>,
    /// The most recent failed or refused run, even if later runs succeeded.
    pub last_failure: Option<Failure>,
    pub next_run_at: Option<DateTime<Utc>>,
    pub consecutive_failures: u32,
    pub last_run: Option<RunRecord>,
    pub push: PushState,
    /// When the last change notification arrived.
    pub last_push_at: Option<DateTime<Utc>>,
    /// Two-way with `sync.watch`: local changes start a sync within seconds. Otherwise they're picked up by the next
    /// scheduled check.
    #[serde(default)]
    pub watching: bool,
    /// Whether local changes are uploaded (two-way and not read-only).
    #[serde(default)]
    pub uploads: bool,
}

/// A snapshot of the whole service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    pub account: Option<String>,
    pub tenant: String,
    pub started_at: DateTime<Utc>,
    pub paused: bool,
    /// Set while SharePoint is throttling us; every request waits until then.
    pub throttled_until: Option<DateTime<Utc>>,
    pub requests: u64,
    pub throttled_responses: u64,
    /// Estimated SharePoint resource units used since the service started (see `RequestKind::ru`).
    #[serde(default)]
    pub ru_estimate: u64,
    #[serde(default)]
    pub requests_by_kind: std::collections::BTreeMap<looper_sharepoint_graph::RequestKind, looper_sharepoint_graph::KindStats>,
    pub poll_interval_secs: u64,
    pub push_safety_interval_secs: u64,
    pub libraries: Vec<LibraryStatus>,
}

impl Status {
    /// Live, polling, or a mix: what the TUI header, tray and `status` show prominently.
    pub fn mode(&self) -> Mode {
        let enabled: Vec<&PushState> = self.libraries.iter().map(|l| &l.push).filter(|p| **p != PushState::Off).collect();
        if enabled.is_empty() {
            return Mode::Off;
        }
        let live = enabled.iter().filter(|p| p.is_live()).count();
        if live == enabled.len() && live == self.libraries.len() {
            return Mode::Live;
        }
        let down = enabled.iter().find_map(|p| match p {
            PushState::Down { since, reason, .. } | PushState::Unavailable { since, reason, .. } => Some((*since, reason.clone())),
            _ => None,
        });
        if live > 0 {
            return Mode::PartlyLive { live, total: self.libraries.len() };
        }
        match down {
            Some((since, reason)) => Mode::Polling { since, reason },
            None => Mode::Connecting,
        }
    }

    /// The most important thing to tell a user, for a tray tooltip or header line.
    pub fn headline(&self) -> String {
        let libs = &self.libraries;
        if libs.iter().any(|l| l.state == LibraryState::SignInRequired) {
            return "Sign-in required".into();
        }
        if let Some(t) = self.throttled_until {
            return format!("SharePoint is throttling requests; waiting until {}", t.with_timezone(&chrono::Local).format("%H:%M:%S"));
        }
        if libs.iter().any(|l| l.state == LibraryState::Refused) {
            return "A sync was refused by the mass-delete guard".into();
        }
        let failing = libs.iter().filter(|l| l.state == LibraryState::Error).count();
        if failing > 0 {
            return format!("{failing} {} failing to sync", if failing == 1 { "library" } else { "libraries" });
        }
        if libs.iter().any(|l| matches!(l.state, LibraryState::Syncing { .. })) {
            return "Syncing…".into();
        }
        if self.paused {
            return "Paused".into();
        }
        let problems: usize = libs.iter().filter_map(|l| l.last_run.as_ref()?.summary.as_ref()).map(|s| s.conflicts + s.errors).sum();
        if problems > 0 {
            return format!("Up to date, {problems} {} need attention", if problems == 1 { "item" } else { "items" });
        }
        if libs.is_empty() {
            return "No libraries configured".into();
        }
        "Up to date".into()
    }

    /// Overall health, for picking a tray icon.
    pub fn health(&self) -> Health {
        let libs = &self.libraries;
        if libs.iter().any(|l| matches!(l.state, LibraryState::SignInRequired | LibraryState::Error | LibraryState::Refused)) {
            Health::Attention
        } else if self.throttled_until.is_some() || self.paused {
            Health::Waiting
        } else if libs.iter().any(|l| matches!(l.state, LibraryState::Syncing { .. })) {
            Health::Syncing
        } else {
            Health::Ok
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Health {
    Ok,
    Syncing,
    Waiting,
    Attention,
}

/// Broadcast to subscribers. Lossy: a slow subscriber may miss events and should re-read the status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    RunStarted { library: String, trigger: Trigger },
    Progress { library: String, progress: Progress },
    RunFinished { record: RunRecord },
    Throttled { until: DateTime<Utc> },
    /// Anything in [`Status`] changed (pause, schedule, sign-in, ...).
    StatusChanged,
}

/// The result of a run requested with [`crate::Service::sync_now`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunResult {
    pub record: RunRecord,
    /// The full report, when the run got far enough to produce one.
    pub report: Option<Report>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lib(push: PushState) -> LibraryStatus {
        LibraryStatus {
            name: "qa".into(),
            local_path: "/tmp/qa".into(),
            site: "s".into(),
            library: "l".into(),
            state: LibraryState::Idle,
            last_success_at: None,
            last_failure: None,
            next_run_at: None,
            consecutive_failures: 0,
            last_run: None,
            push,
            last_push_at: None,
            watching: false,
            uploads: true,
        }
    }

    fn status(push: Vec<PushState>) -> Status {
        Status {
            account: None,
            tenant: "t".into(),
            started_at: Utc::now(),
            paused: false,
            throttled_until: None,
            requests: 0,
            throttled_responses: 0,
            ru_estimate: 0,
            requests_by_kind: Default::default(),
            poll_interval_secs: 45,
            push_safety_interval_secs: 900,
            libraries: push.into_iter().map(lib).collect(),
        }
    }

    #[test]
    fn mode_summarizes_libraries() {
        let now = Utc::now();
        let live = || PushState::Live { since: now };
        let down = || PushState::Down { since: now, reason: "heartbeat timeout".into(), retry_at: None };
        let connecting = || PushState::Connecting { since: now, attempt: 0 };
        assert_eq!(status(vec![live(), live()]).mode(), Mode::Live);
        assert_eq!(status(vec![live(), down()]).mode(), Mode::PartlyLive { live: 1, total: 2 });
        assert_eq!(status(vec![live(), PushState::Off]).mode(), Mode::PartlyLive { live: 1, total: 2 });
        assert_eq!(status(vec![down(), connecting()]).mode(), Mode::Polling { since: now, reason: "heartbeat timeout".into() });
        assert_eq!(status(vec![connecting()]).mode(), Mode::Connecting);
        assert_eq!(status(vec![PushState::Off]).mode(), Mode::Off);
        assert_eq!(status(vec![]).mode(), Mode::Off);
        assert_eq!(Mode::Off.describe(45), "Live updates off · checking every 45s");
        assert_eq!(Mode::Polling { since: now, reason: String::new() }.describe(120), "Live updates down · checking every 2m");
    }
}
