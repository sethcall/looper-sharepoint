//! Human-readable formatting shared by the one-shot commands, `status` and the TUI.

use chrono::{DateTime, Utc};
use looper_sharepoint_engine::{LibraryState, LibraryStatus, Mode, PushState, Report, RunRecord, Status};

pub fn human_bytes(n: u64) -> String {
    const U: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{n} B") } else { format!("{v:.1} {}", U[i]) }
}

/// "12s", "4m", "3h", "2d".
pub fn short_duration(secs: i64) -> String {
    let s = secs.max(0);
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m", s / 60),
        3600..86400 => format!("{}h{:02}m", s / 3600, s % 3600 / 60),
        _ => format!("{}d", s / 86400),
    }
}

/// "12s ago", or "never".
pub fn ago(t: Option<DateTime<Utc>>, now: DateTime<Utc>) -> String {
    match t {
        Some(t) => format!("{} ago", short_duration((now - t).num_seconds())),
        None => "never".into(),
    }
}

/// "in 31s", "now", or "–".
pub fn until(t: Option<DateTime<Utc>>, now: DateTime<Utc>) -> String {
    match t {
        Some(t) if t <= now => "now".into(),
        Some(t) => format!("in {}", short_duration((t - now).num_seconds() + 1)),
        None => "–".into(),
    }
}

pub fn local_time(t: DateTime<Utc>) -> String {
    t.with_timezone(&chrono::Local).format("%H:%M:%S").to_string()
}

pub fn state_text(l: &LibraryStatus, paused: bool) -> String {
    match &l.state {
        LibraryState::Idle if paused => "paused".into(),
        LibraryState::Idle => match l.last_run.as_ref().and_then(|r| r.summary.as_ref()) {
            Some(s) if s.conflicts + s.errors > 0 => format!("idle, {} need attention", s.conflicts + s.errors),
            _ => "idle".into(),
        },
        LibraryState::Syncing { total, done, .. } if *total > 0 => format!("syncing {done}/{total}"),
        LibraryState::Syncing { .. } => "syncing".into(),
        LibraryState::Error => format!("retrying ({} failures)", l.consecutive_failures),
        LibraryState::SignInRequired => "sign-in required".into(),
        LibraryState::Refused => "refused (mass delete)".into(),
    }
}

/// The prominent mode line: are changes arriving live, or is the client polling (and why)?
pub fn mode_line(s: &Status, now: DateTime<Utc>) -> String {
    let every = short_duration(s.poll_interval_secs as i64);
    match s.mode() {
        Mode::Live => "● LIVE · changes arrive within ~30s".into(),
        Mode::PartlyLive { live, total } => format!("◐ PARTLY LIVE · {live} of {total} libraries live, others checked every {every}"),
        Mode::Connecting => "◌ CONNECTING live updates…".into(),
        Mode::Polling { since, reason } => {
            format!("◐ POLLING every {every} · live updates down {}: {}", short_duration((now - since).num_seconds()), first_line(&reason))
        }
        Mode::Off => format!("○ POLLING every {every} · live updates off in config"),
    }
}

/// How local edits reach SharePoint for this library.
pub fn local_changes_text(l: &LibraryStatus) -> &'static str {
    match (l.uploads, l.watching) {
        (false, _) => "not uploaded (download-only or read-only)",
        (true, true) => "uploaded within seconds (watching the folder)",
        (true, false) => "uploaded at the next check",
    }
}

/// Glyph for a library's live-updates state.
pub fn push_glyph(p: &PushState) -> &'static str {
    match p {
        PushState::Live { .. } => "●",
        PushState::Connecting { .. } => "◌",
        PushState::Down { .. } | PushState::Unavailable { .. } => "◐",
        PushState::Off => "○",
    }
}

/// A library's live-updates state, e.g. "● live for 2h14m" or "◐ down 2m: heartbeat timeout · retry in 12s".
pub fn push_text(l: &LibraryStatus, now: DateTime<Utc>) -> String {
    let g = push_glyph(&l.push);
    let retry = |t: Option<DateTime<Utc>>| match t {
        Some(t) => format!(" · retry {}", until(Some(t), now)),
        None => String::new(),
    };
    match &l.push {
        PushState::Off => format!("{g} off (polling)"),
        PushState::Connecting { attempt: 0, .. } => format!("{g} connecting…"),
        PushState::Connecting { attempt, .. } => format!("{g} reconnecting (attempt {})", attempt + 1),
        PushState::Live { since } => format!("{g} live for {}", short_duration((now - *since).num_seconds())),
        PushState::Down { reason, retry_at: None, .. } => format!("{g} paused: {reason}"),
        PushState::Down { since, reason, retry_at } => {
            format!("{g} down {}: {}{}", short_duration((now - *since).num_seconds()), first_line(reason), retry(*retry_at))
        }
        PushState::Unavailable { reason, retry_at, .. } => format!("{g} unavailable: {}{}", first_line(reason), retry(Some(*retry_at))),
    }
}

pub fn failure_text(l: &LibraryStatus, now: DateTime<Utc>) -> String {
    match &l.last_failure {
        Some(f) => format!("{}: {}", ago(Some(f.at), now), first_line(&f.error)),
        None => "never".into(),
    }
}

pub fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or("")
}

pub fn run_line(r: &RunRecord) -> String {
    format!(
        "{}  {:<12} {:<7} {:<8} {}  ({:.1}s)",
        local_time(r.started_at),
        r.library,
        r.trigger.as_str(),
        r.outcome.as_str(),
        first_line(&r.describe()),
        r.duration_ms() as f64 / 1000.0
    )
}

/// The `status` command's text output (live or offline).
pub fn print_status(s: &Status, history: &[RunRecord], live: bool) {
    let now = Utc::now();
    let who = s.account.as_deref().unwrap_or("not signed in");
    if live {
        println!("Host running for {} as {who}: {}", short_duration((now - s.started_at).num_seconds()), s.headline());
        println!("mode: {}", mode_line(s, now));
        let hours = ((now - s.started_at).num_seconds().max(1) as f64) / 3600.0;
        println!(
            "requests: {} since start, ~{} RU (est.), ~{:.0} RU/day at this rate; throttled {}",
            s.requests,
            s.ru_estimate,
            s.ru_estimate as f64 / hours * 24.0,
            s.throttled_responses
        );
        if let Some(t) = s.throttled_until {
            println!("  SharePoint is throttling: all requests wait until {}", local_time(t));
        }
    } else {
        println!("No host running (offline view, as {who}). Start one with `looper-sharepoint run`.");
    }
    for l in &s.libraries {
        println!("{}  ({})", l.name, l.local_path);
        if live {
            println!("  state:        {}", state_text(l, s.paused));
            println!("  live updates: {}", push_text(l, now));
            if let Some(t) = l.last_push_at {
                println!("  last change notification: {}", ago(Some(t), now));
            }
            println!("  next check:   {}", until(l.next_run_at, now));
            println!("  local changes: {}", local_changes_text(l));
        }
        println!("  last sync:    {}", ago(l.last_success_at, now));
        println!("  last failure: {}", failure_text(l, now));
        if let Some(run) = &l.last_run {
            for i in &run.issues {
                println!("  ! {}: {}", i.path, i.reason);
            }
        }
    }
    if !history.is_empty() {
        println!("Recent runs:");
        for r in history {
            println!("  {}", run_line(r));
        }
    }
}

pub fn print_report(r: &Report, took: std::time::Duration) {
    let mode = if r.dry_run { " (dry run: nothing changed)" } else { "" };
    let kind = if r.resynced {
        "full re-enumeration (delta token expired)"
    } else if r.full_enumeration {
        "full enumeration"
    } else {
        "incremental"
    };
    println!("{} → {}{mode}", r.library, r.local_root);
    println!("  {kind}, {} changes from SharePoint, {:.1}s", r.delta_items, took.as_secs_f64());
    println!(
        "  downloaded {} ({}), adopted {}, moved {}, folders created {}, deleted locally {}",
        r.downloaded,
        human_bytes(r.bytes_downloaded),
        r.adopted,
        r.moved,
        r.folders_created,
        r.deleted_local.len()
    );
    let list = |title: &str, items: Vec<String>| {
        if !items.is_empty() {
            println!("  {title} ({}):", items.len());
            for i in items.iter().take(20) {
                println!("    {i}");
            }
            if items.len() > 20 {
                println!("    … and {} more", items.len() - 20);
            }
        }
    };
    if r.uploaded + r.created_remote + r.moved_remote + r.deleted_remote.len() > 0 {
        println!(
            "  uploaded {} ({}), folders created in SharePoint {}, moved in SharePoint {}, deleted in SharePoint {}",
            r.uploaded,
            human_bytes(r.bytes_uploaded),
            r.created_remote,
            r.moved_remote,
            r.deleted_remote.len()
        );
    }
    list("deleted locally, as they were deleted in SharePoint (recycle bin) or are no longer shared with you", r.deleted_local.clone());
    list("deleted in SharePoint (recoverable from the site recycle bin)", r.deleted_remote.clone());
    list("conflict copies kept (your version, also uploaded)", r.conflict_copies.clone());
    list("CONFLICTS (not overwritten)", r.conflicts.iter().map(|i| format!("{}: {}", i.path, i.reason)).collect());
    list("ERRORS (will retry next run)", r.errors.iter().map(|i| format!("{}: {}", i.path, i.reason)).collect());
    list("skipped", r.skipped.iter().map(|i| format!("{}: {}", i.path, i.reason)).collect());
    list("local edits kept (not uploaded: download-only or read-only, or re-uploaded after a SharePoint delete)", r.local_edits_kept.clone());
    list("local-only files (outside this library's include/exclude filter; not uploaded)", r.local_only.clone());
    if r.orphans_deleted > 0 {
        println!("  local-only files deleted (sync.local_orphans = \"delete\"): {}", r.orphans_deleted);
    } else {
        list("local-only files (kept)", r.orphans.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(short_duration(5), "5s");
        assert_eq!(short_duration(125), "2m");
        assert_eq!(short_duration(3 * 3600 + 5 * 60), "3h05m");
        assert_eq!(short_duration(3 * 86400), "3d");
        let now = Utc::now();
        assert_eq!(ago(Some(now - chrono::Duration::seconds(12)), now), "12s ago");
        assert_eq!(until(Some(now + chrono::Duration::seconds(30)), now), "in 31s");
        assert_eq!(until(None, now), "–");
    }
}
