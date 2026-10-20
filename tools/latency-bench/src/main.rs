//! Sync latency against a running host. See `specs/m2-design.md` §8 and `specs/m4-design.md` §8.
//!
//! `--direction down` (default, spike criterion 3): SharePoint → local.
//! 1. Subscribe to the host's events over its control socket.
//! 2. Create, edit, rename and delete files under PREFIX in SharePoint at random intervals.
//! 3. For each mutation, wait for the host's progress event that applied it locally (download, move or delete).
//! 4. Write a CSV and print p50 / p95 / max.
//!
//! `--direction up`: local → SharePoint. The same, but the mutations happen in the host's local library folder (found
//! from its status), and a mutation counts as arrived when the host reports it done in SharePoint (upload, move or
//! delete there). The host must be two-way with the watcher on.
//!
//! The host's config must include PREFIX (e.g. `include = ["runs/**"]`) with no name mapping needed. Credentials for
//! the mutating client come from `.env`, as for the fixtures tool.

use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use clap::Parser;
use looper_sharepoint_engine::auth::{Authority, PasswordProvider};
use looper_sharepoint_engine::graph::GraphClient;
use looper_sharepoint_engine::{Event, Mode, Progress};
use looper_sharepoint_ipc::{Client, Request, Response};

#[derive(Parser)]
#[command(name = "looper-sharepoint-latency-bench")]
struct Cli {
    /// Folder in the QA library to work in (created; moved to the recycle bin at the end)
    #[arg(long)]
    prefix: Option<String>,
    /// The host's library (`local` name) that covers PREFIX
    #[arg(long, default_value = "qa")]
    library: String,
    #[arg(long, default_value_t = 200)]
    mutations: usize,
    /// Random gap between mutations, in seconds
    #[arg(long, default_value_t = 2.0)]
    min_gap: f64,
    #[arg(long, default_value_t = 15.0)]
    max_gap: f64,
    /// Give up on a mutation after this many seconds
    #[arg(long, default_value_t = 300)]
    timeout: u64,
    /// The host's data dir (default: LOOPER_SHAREPOINT_DATA_DIR or the OS default)
    #[arg(long)]
    data_dir: Option<PathBuf>,
    #[arg(long, default_value = ".env")]
    env: String,
    #[arg(long, default_value = "latency.csv")]
    out: PathBuf,
    /// Fail unless the host's live updates stayed connected for the whole run
    #[arg(long)]
    expect_live: bool,
    /// down: SharePoint → local; up: local → SharePoint
    #[arg(long, value_enum, default_value_t = Direction::Down)]
    direction: Direction,
}

#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum Direction {
    Down,
    Up,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Kind {
    Create,
    Edit,
    Rename,
    Delete,
}

struct Mutation {
    kind: Kind,
    /// Local path (relative to the library folder) whose event proves the change arrived.
    expect: String,
    at: Instant,
    at_wall: chrono::DateTime<chrono::Utc>,
    observed: Option<Instant>,
}

/// Paths the host reported, with when.
type Seen = Arc<Mutex<Vec<(String, Instant)>>>;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let env = load_env(&cli.env);
    let get = |k: &str| env.get(k).cloned().with_context(|| format!("{k} not set (in env or {})", cli.env));
    let data_dir = cli.data_dir.clone().unwrap_or_else(looper_sharepoint_engine::core::config::data_dir);
    let auth = Authority {
        tenant: get("TENANT_ID")?,
        client_id: get("APP_CLIENT_ID")?,
        scopes: vec!["https://graph.microsoft.com/.default".into()],
    };
    let g = GraphClient::new(PasswordProvider::new(auth, get("QA_USER_1")?, get("QA_USER_1_PASSWORD")?));
    let drive = get("QA_DRIVE_ID")?;
    let prefix = cli.prefix.clone().unwrap_or_else(|| format!("runs/latency-{}", chrono::Utc::now().format("%Y%m%d-%H%M%S")));

    // Listen first, so nothing is missed.
    let mut sub = Client::connect(&data_dir)
        .await?
        .with_context(|| format!("no looper-sharepoint host running for {}", data_dir.display()))?
        .subscribe()
        .await?;
    let seen: Seen = Arc::default();
    {
        let (seen, lib, direction) = (seen.clone(), cli.library.clone(), cli.direction);
        tokio::spawn(async move {
            while let Ok(Some(resp)) = sub.next().await {
                if let Response::Event { event: Event::Progress { library, progress } } = resp
                    && library.eq_ignore_ascii_case(&lib)
                {
                    let path = match (direction, progress) {
                        (Direction::Down, Progress::Downloaded { path } | Progress::Deleted { path }) => path,
                        (Direction::Down, Progress::Moved { to, .. }) => to,
                        (Direction::Up, Progress::Uploaded { path } | Progress::DeletedRemote { path }) => path,
                        (Direction::Up, Progress::MovedRemote { to, .. }) => to,
                        _ => continue,
                    };
                    seen.lock().unwrap().push((path, Instant::now()));
                }
            }
        });
    }

    // Sample the host's mode (live vs polling) every few seconds, so the result says which one it measured.
    let modes: Arc<Mutex<Vec<String>>> = Arc::default();
    {
        let (modes, data_dir) = (modes.clone(), data_dir.clone());
        tokio::spawn(async move {
            loop {
                if let Ok(Some(Response::Status { status, .. })) = looper_sharepoint_ipc::request(&data_dir, &Request::Status { history: 0 }).await {
                    let m = match status.mode() {
                        Mode::Live => "live".to_string(),
                        m => m.describe(status.poll_interval_secs),
                    };
                    let mut v = modes.lock().unwrap();
                    if v.last() != Some(&m) {
                        eprintln!("  host mode: {m}");
                        v.push(m);
                    }
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        });
    }
    // Up: the host's local folder for the library.
    let local_root = match cli.direction {
        Direction::Down => None,
        Direction::Up => {
            let Some(Response::Status { status, .. }) = looper_sharepoint_ipc::request(&data_dir, &Request::Status { history: 0 }).await? else {
                bail!("host did not answer status");
            };
            let lib = status.libraries.iter().find(|l| l.name.eq_ignore_ascii_case(&cli.library)).context("library not in host")?;
            if !lib.uploads || !lib.watching {
                bail!("--direction up needs a two-way host that is watching its folder (uploads={}, watching={})", lib.uploads, lib.watching);
            }
            Some(PathBuf::from(&lib.local_path))
        }
    };
    let local = |rel: &str| -> PathBuf {
        let mut p = local_root.clone().unwrap_or_default();
        for seg in rel.split('/') {
            p.push(seg);
        }
        p
    };
    eprintln!("benchmarking {} mutations under {prefix} (host library {:?}, direction {})", cli.mutations, cli.library,
        if cli.direction == Direction::Up { "up" } else { "down" });
    let mut muts: Vec<Mutation> = vec![];
    // Files we may touch: remote path -> (item id, index of its latest mutation).
    let mut files: HashMap<String, (String, usize)> = HashMap::new();
    let mut n = 0usize;
    let started = Instant::now();
    while muts.len() < cli.mutations {
        let gap = cli.min_gap + fastrand::f64() * (cli.max_gap - cli.min_gap);
        tokio::time::sleep(Duration::from_secs_f64(gap)).await;
        mark_observed(&mut muts, &seen);
        // Only edit/rename/delete files whose last change has already arrived, so every mutation has exactly one
        // unambiguous expected event.
        let ready: Vec<String> = files.iter().filter(|(_, (_, i))| muts[*i].observed.is_some()).map(|(p, _)| p.clone()).collect();
        let roll = fastrand::u32(0..100);
        let kind = match roll {
            _ if ready.is_empty() || files.len() < 3 => Kind::Create,
            0..35 => Kind::Create,
            35..70 => Kind::Edit,
            70..85 => Kind::Rename,
            _ => Kind::Delete,
        };
        let content = Bytes::from(format!("latency bench {n} at {}\n", chrono::Utc::now().to_rfc3339()));
        let (expect, at) = match (cli.direction, kind) {
            (Direction::Up, Kind::Create) => {
                n += 1;
                let path = format!("{prefix}/f{n:04}.txt");
                write_file(&local(&path), &content)?;
                files.insert(path.clone(), (String::new(), muts.len()));
                (path, Instant::now())
            }
            (Direction::Up, Kind::Edit) => {
                let path = ready[fastrand::usize(..ready.len())].clone();
                write_file(&local(&path), &content)?;
                files.get_mut(&path).unwrap().1 = muts.len();
                (path, Instant::now())
            }
            (Direction::Up, Kind::Rename) => {
                let old = ready[fastrand::usize(..ready.len())].clone();
                files.remove(&old);
                n += 1;
                let new = format!("{prefix}/r{n:04}.txt");
                std::fs::rename(local(&old), local(&new))?;
                files.insert(new.clone(), (String::new(), muts.len()));
                (new, Instant::now())
            }
            (Direction::Up, Kind::Delete) => {
                let path = ready[fastrand::usize(..ready.len())].clone();
                files.remove(&path);
                std::fs::remove_file(local(&path))?;
                (path, Instant::now())
            }
            (Direction::Down, Kind::Create) => {
                n += 1;
                let path = format!("{prefix}/f{n:04}.txt");
                let item = g.upload(&drive, &path, content).await?;
                let at = Instant::now();
                files.insert(path.clone(), (item.id, muts.len()));
                (path, at)
            }
            (Direction::Down, Kind::Edit) => {
                let path = ready[fastrand::usize(..ready.len())].clone();
                g.upload(&drive, &path, content).await?;
                let at = Instant::now();
                files.get_mut(&path).unwrap().1 = muts.len();
                (path, at)
            }
            (Direction::Down, Kind::Rename) => {
                let old = ready[fastrand::usize(..ready.len())].clone();
                let (id, _) = files.remove(&old).unwrap();
                n += 1;
                let new_name = format!("r{n:04}.txt");
                g.move_item(&drive, &id, None, Some(&new_name)).await?;
                let at = Instant::now();
                let new = format!("{prefix}/{new_name}");
                files.insert(new.clone(), (id, muts.len()));
                (new, at)
            }
            (Direction::Down, Kind::Delete) => {
                let path = ready[fastrand::usize(..ready.len())].clone();
                let (id, _) = files.remove(&path).unwrap();
                g.delete_item(&drive, &id).await?;
                (path, Instant::now())
            }
        };
        muts.push(Mutation { kind, expect, at, at_wall: chrono::Utc::now(), observed: None });
        if muts.len().is_multiple_of(20) {
            let done = muts.iter().filter(|m| m.observed.is_some()).count();
            eprintln!("  {} mutations, {done} observed, {:.0}s elapsed", muts.len(), started.elapsed().as_secs_f64());
        }
    }

    // Wait for the stragglers.
    let deadline = Instant::now() + Duration::from_secs(cli.timeout);
    loop {
        mark_observed(&mut muts, &seen);
        if muts.iter().all(|m| m.observed.is_some()) || Instant::now() > deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    write_csv(&cli.out, &muts)?;
    report(&muts);
    let modes = modes.lock().unwrap().clone();
    let live_throughout = modes == ["live"];
    println!("host mode during the run: {}", if modes.is_empty() { "unknown".into() } else { modes.join(" → ") });

    // Clean up (recycle bin), then let the host apply that deletion locally: it is usually most of the files the host
    // tracks, which the mass-delete guard would (rightly) refuse to mirror without the override.
    if let Ok(root) = g.item_by_path(&drive, &prefix).await {
        g.delete_item(&drive, &root.id).await?;
        let opts = looper_sharepoint_engine::RunOptions { allow_mass_delete: true, ..Default::default() };
        let req = Request::Sync { library: Some(cli.library.clone()), options: opts };
        match looper_sharepoint_ipc::request(&data_dir, &req).await {
            Ok(Some(Response::Sync { .. })) => eprintln!("cleaned up {prefix} (SharePoint recycle bin and host)"),
            other => eprintln!("cleaned up {prefix} in SharePoint; host sync afterwards: {other:?}"),
        }
    }
    if muts.iter().any(|m| m.observed.is_none()) {
        bail!("some mutations were never observed");
    }
    if cli.expect_live && !live_throughout {
        bail!("--expect-live: live updates were not connected for the whole run");
    }
    Ok(())
}

/// Match each pending mutation to the first event for its path after it happened.
fn mark_observed(muts: &mut [Mutation], seen: &Seen) {
    let seen = seen.lock().unwrap();
    for m in muts.iter_mut().filter(|m| m.observed.is_none()) {
        m.observed = seen.iter().find(|(p, t)| *p == m.expect && *t >= m.at).map(|(_, t)| *t);
    }
}

fn write_file(p: &std::path::Path, content: &[u8]) -> Result<()> {
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d)?;
    }
    std::fs::write(p, content)?;
    Ok(())
}

fn write_csv(path: &PathBuf, muts: &[Mutation]) -> Result<()> {
    let mut f = std::fs::File::create(path)?;
    writeln!(f, "seq,kind,path,mutated_at,latency_s")?;
    for (i, m) in muts.iter().enumerate() {
        let lat = m.observed.map(|o| format!("{:.3}", (o - m.at).as_secs_f64())).unwrap_or_default();
        writeln!(f, "{i},{:?},{},{},{lat}", m.kind, m.expect, m.at_wall.to_rfc3339())?;
    }
    eprintln!("wrote {}", path.display());
    Ok(())
}

fn report(muts: &[Mutation]) {
    let stats = |label: &str, ms: Vec<&Mutation>| {
        let mut v: Vec<f64> = ms.iter().filter_map(|m| m.observed.map(|o| (o - m.at).as_secs_f64())).collect();
        let missing = ms.len() - v.len();
        if v.is_empty() {
            println!("{label:<8} n={:<4} (none observed)", ms.len());
            return;
        }
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let pct = |p: f64| v[((p * (v.len() - 1) as f64).round() as usize).min(v.len() - 1)];
        println!(
            "{label:<8} n={:<4} p50={:>6.1}s  p95={:>6.1}s  max={:>6.1}s  missing={missing}",
            ms.len(),
            pct(0.5),
            pct(0.95),
            v[v.len() - 1]
        );
    };
    stats("all", muts.iter().collect());
    for k in [Kind::Create, Kind::Edit, Kind::Rename, Kind::Delete] {
        stats(&format!("{k:?}").to_lowercase(), muts.iter().filter(|m| m.kind == k).collect());
    }
}

fn load_env(path: &str) -> HashMap<String, String> {
    let mut vars = HashMap::new();
    if let Ok(text) = std::fs::read_to_string(path) {
        for line in text.lines().map(str::trim).filter(|l| !l.starts_with('#')) {
            if let Some((k, v)) = line.split_once('=') {
                let v = v.split(" #").next().unwrap_or(v).trim().trim_matches('\'').trim_matches('"');
                vars.insert(k.trim().to_string(), v.to_string());
            }
        }
    }
    vars.extend(std::env::vars());
    vars
}
