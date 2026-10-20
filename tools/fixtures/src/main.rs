//! Test data in the QA library: seed, mutate, clean. Each run works under its own prefix (e.g. `runs/<id>`).
//!
//! Reads `TENANT_ID`, `APP_CLIENT_ID`, `QA_USER_1`, `QA_USER_1_PASSWORD`, `QA_DRIVE_ID` from the environment or `.env`.
//! Signs in with the password grant (the QA account has no MFA).

use std::collections::HashMap;


use anyhow::{Context, Result};
use bytes::Bytes;
use clap::{Parser, Subcommand};
use futures_util::StreamExt;
use looper_sharepoint_auth::{Authority, PasswordProvider};
use looper_sharepoint_graph::GraphClient;
use looper_sharepoint_graph::models::DriveItem;

#[derive(Parser)]
#[command(name = "looper-sharepoint-fixtures")]
struct Cli {
    /// Path to the .env file
    #[arg(long, default_value = ".env")]
    env: String,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create a tree of files under PREFIX
    Seed {
        prefix: String,
        #[arg(long, default_value_t = 100)]
        files: usize,
        /// Number of large files
        #[arg(long, default_value_t = 0)]
        large: usize,
        /// Size of each large file in MB
        #[arg(long, default_value_t = 150)]
        large_mb: usize,
        /// Folders per level
        #[arg(long, default_value_t = 4)]
        fanout: usize,
        #[arg(long, default_value_t = 8)]
        parallel: usize,
    },
    /// Randomly edit/delete/rename/move files under PREFIX
    Mutate {
        prefix: String,
        #[arg(long, default_value_t = 0)]
        edits: usize,
        #[arg(long, default_value_t = 0)]
        deletes: usize,
        #[arg(long, default_value_t = 0)]
        renames: usize,
        /// Rename a folder (moves its whole subtree)
        #[arg(long, default_value_t = 0)]
        folder_renames: usize,
        #[arg(long, default_value_t = 0)]
        creates: usize,
    },
    /// Upload one file with given content
    Put { path: String, content: String },
    /// Delete PREFIX (to the recycle bin)
    Clean { prefix: String },
    /// List files under PREFIX with their quickXorHash
    List { prefix: String },
    /// Print an item's id, eTag and cTag
    Etag { path: String },
    /// Compare PREFIX in SharePoint with a local folder (LOCAL corresponds to the library root)
    Verify { prefix: String, local: std::path::PathBuf },
    /// Connect to the drive's socket.io endpoint and log raw frames (M3 probe; see specs/m3-design.md §7)
    PushProbe {
        /// Engine.IO protocol version to request
        #[arg(long, default_value_t = 4)]
        eio: u8,
        /// Join the URL path as the socket.io namespace (else connect at that path)
        #[arg(long)]
        no_namespace: bool,
        /// Stop after this many seconds
        #[arg(long, default_value_t = 120)]
        secs: u64,
        /// Write a file under PREFIX every N seconds (0 = never), and time notification and delta visibility
        #[arg(long, default_value_t = 0)]
        mutate_every: u64,
        #[arg(long, default_value = "runs/push-probe")]
        prefix: String,
    },
    /// Send one Graph request and print the response (for probing APIs). PATH is relative to /v1.0; `{drive}` is replaced.
    Raw {
        method: String,
        path: String,
        /// JSON request body
        #[arg(long)]
        body: Option<String>,
        /// Use the beta endpoint
        #[arg(long)]
        beta: bool,
    },
    /// Two sockets on the same user/app/drive subscription: does closing one stop notifications to the other?
    PushShare {
        /// Close socket B by dropping the connection instead of sending a socket.io disconnect
        #[arg(long)]
        drop: bool,
        /// Seconds to wait for notifications after each write
        #[arg(long, default_value_t = 75)]
        wait: u64,
        #[arg(long, default_value = "runs/push-share")]
        prefix: String,
    },
}

struct Env {
    vars: HashMap<String, String>,
}

impl Env {
    fn load(path: &str) -> Self {
        let mut vars: HashMap<String, String> = HashMap::new();
        if let Ok(text) = std::fs::read_to_string(path) {
            for line in text.lines() {
                let line = line.trim();
                if line.starts_with('#') || !line.contains('=') {
                    continue;
                }
                let (k, v) = line.split_once('=').unwrap();
                let v = v.split(" #").next().unwrap_or(v).trim().trim_matches('\'').trim_matches('"');
                vars.insert(k.trim().to_string(), v.to_string());
            }
        }
        for (k, v) in std::env::vars() {
            vars.insert(k, v);
        }
        Self { vars }
    }
    fn get(&self, k: &str) -> Result<String> {
        self.vars.get(k).cloned().with_context(|| format!("{k} not set (in env or .env)"))
    }
}

/// Small deterministic PRNG so fixtures need no extra dependency and runs are reproducible from a seed.
struct Rng(u64);
impl Rng {
    fn seeded() -> Self {
        let mut b = [0u8; 8];
        getrandom::fill(&mut b).unwrap();
        Rng(u64::from_le_bytes(b) | 1)
    }
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    fn bytes(&mut self, n: usize) -> Vec<u8> {
        let mut v = Vec::with_capacity(n + 8);
        while v.len() < n {
            v.extend_from_slice(&self.next().to_le_bytes());
        }
        v.truncate(n);
        v
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let env = Env::load(&cli.env);
    let auth = Authority {
        tenant: env.get("TENANT_ID")?,
        client_id: env.get("APP_CLIENT_ID")?,
        scopes: vec!["https://graph.microsoft.com/.default".into()],
    };
    let tokens = PasswordProvider::new(auth, env.get("QA_USER_1")?, env.get("QA_USER_1_PASSWORD")?);
    let g = GraphClient::new(tokens);
    let drive = env.get("QA_DRIVE_ID")?;

    match cli.cmd {
        Cmd::Seed { prefix, files, large, large_mb, fanout, parallel } => seed(&g, &drive, &prefix, files, large, large_mb, fanout, parallel).await?,
        Cmd::Mutate { prefix, edits, deletes, renames, folder_renames, creates } => {
            mutate(&g, &drive, &prefix, edits, deletes, renames, folder_renames, creates).await?
        }
        Cmd::Put { path, content } => {
            let it = g.upload(&drive, &path, Bytes::from(content)).await?;
            println!("{} {}", path, it.quickxor().unwrap_or("-"));
        }
        Cmd::Clean { prefix } => {
            match g.item_by_path(&drive, &prefix).await {
                Ok(it) => {
                    g.delete_item(&drive, &it.id).await?;
                    println!("deleted {prefix} (to recycle bin)");
                }
                Err(looper_sharepoint_graph::GraphError::NotFound(_)) => println!("{prefix} does not exist"),
                Err(e) => return Err(e.into()),
            }
        }
        Cmd::Verify { prefix, local } => {
            let remote = list(&g, &drive, &prefix).await?;
            let (mut ok, mut bad) = (0, 0);
            let mut expected = std::collections::HashSet::new();
            for (path, it) in &remote {
                if it.folder.is_some() {
                    continue;
                }
                let abs = local.join(path);
                expected.insert(abs.clone());
                match looper_sharepoint_core::quickxor::hash_file(&abs) {
                    Ok(h) if Some(h.as_str()) == it.quickxor() => ok += 1,
                    Ok(h) => {
                        bad += 1;
                        println!("MISMATCH {path}: local {h}, remote {}", it.quickxor().unwrap_or("-"));
                    }
                    Err(e) => {
                        bad += 1;
                        println!("MISSING  {path}: {e}");
                    }
                }
            }
            let mut extra = 0;
            let mut stack = vec![local.join(&prefix)];
            while let Some(d) = stack.pop() {
                for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
                    let p = e.path();
                    if p.is_dir() {
                        stack.push(p);
                    } else if !expected.contains(&p) {
                        extra += 1;
                        println!("EXTRA    {}", p.display());
                    }
                }
            }
            println!("verify: {ok} match, {bad} missing/mismatched, {extra} extra local files");
            if bad + extra > 0 {
                std::process::exit(1);
            }
        }
        Cmd::PushProbe { eio, no_namespace, secs, mutate_every, prefix } => {
            push_probe(&g, &drive, eio, !no_namespace, secs, mutate_every, &prefix).await?
        }
        Cmd::Raw { method, path, body, beta } => {
            let path = path.replace("{drive}", &drive);
            let path = if beta { format!("https://graph.microsoft.com/beta{path}") } else { path };
            let m = reqwest::Method::from_bytes(method.to_uppercase().as_bytes())?;
            match g.send(m, &path, |r| match &body { Some(b) => r.header("Content-Type", "application/json").body(b.clone()), None => r }).await {
                Ok(resp) => {
                    let status = resp.status();
                    let text = resp.text().await.unwrap_or_default();
                    let pretty = serde_json::from_str::<serde_json::Value>(&text).map(|v| serde_json::to_string_pretty(&v).unwrap()).unwrap_or(text);
                    println!("{status}\n{pretty}");
                }
                Err(e) => println!("ERROR {e}"),
            }
        }
        Cmd::PushShare { drop, wait, prefix } => push_share(&g, &drive, drop, wait, &prefix).await?,
        Cmd::Etag { path } => {
            let it = g.item_by_path(&drive, &path).await?;
            println!("{}\teTag={}\tcTag={}", it.id, it.e_tag.unwrap_or_default(), it.c_tag.unwrap_or_default());
        }
        Cmd::List { prefix } => {
            for (path, it) in list(&g, &drive, &prefix).await? {
                if it.folder.is_none() {
                    println!("{}\t{}\t{}", path, it.size.unwrap_or(0), it.quickxor().unwrap_or("-"));
                }
            }
        }
    }
    eprintln!("Graph requests: {}", g.stats().requests);
    Ok(())
}

async fn push_share(g: &GraphClient, drive: &str, drop: bool, wait: u64, prefix: &str) -> Result<()> {
    use looper_sharepoint_graph::push::PushSocket;
    use std::time::{Duration, Instant};

    async fn open(g: &GraphClient, drive: &str, name: &str) -> Result<PushSocket> {
        let sub = g.socket_io_endpoint(drive).await?;
        eprintln!("{name}: subscription id={:?} expires={:?}", sub.id, sub.expiration_date_time);
        Ok(PushSocket::connect(&sub.notification_url).await?)
    }
    // Write one file, then report which sockets are notified within `wait`.
    async fn round(g: &GraphClient, drive: &str, path: &str, wait: u64, a: &mut PushSocket, mut b: Option<&mut PushSocket>) -> Result<(bool, bool)> {
        g.upload(drive, path, bytes::Bytes::from_static(b"push-share\n")).await?;
        let t0 = Instant::now();
        eprintln!("wrote {path}");
        let (mut got_a, mut got_b) = (false, false);
        let deadline = tokio::time::sleep(Duration::from_secs(wait));
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                _ = &mut deadline => break,
                r = a.next(), if !got_a => { r?; got_a = true; eprintln!("  A notified after {:.1}s", t0.elapsed().as_secs_f64()); }
                r = async { b.as_deref_mut().unwrap().next().await }, if b.is_some() && !got_b => {
                    r?; got_b = true; eprintln!("  B notified after {:.1}s", t0.elapsed().as_secs_f64());
                }
            }
            if got_a && (got_b || b.is_none()) { break; }
        }
        Ok((got_a, got_b))
    }

    let mut a = open(g, drive, "A").await?;
    let mut b = open(g, drive, "B").await?;
    tokio::time::sleep(Duration::from_secs(5)).await;
    let r1 = round(g, drive, &format!("{prefix}/1-both-open.txt"), wait, &mut a, Some(&mut b)).await?;
    if drop { std::mem::drop(b); } else { b.close().await; }
    eprintln!("B {}", if drop { "dropped (no disconnect packet)" } else { "closed (socket.io disconnect)" });
    tokio::time::sleep(Duration::from_secs(5)).await;
    let r2 = round(g, drive, &format!("{prefix}/2-after-b-closed.txt"), wait, &mut a, None).await?;
    let mut a2 = open(g, drive, "A again").await?;
    tokio::time::sleep(Duration::from_secs(5)).await;
    let r3 = round(g, drive, &format!("{prefix}/3-a-reopened.txt"), wait, &mut a2, None).await?;
    println!("both open:        A={} B={}", r1.0, r1.1);
    println!("after B {}: A={}", if drop { "dropped" } else { "closed " }, r2.0);
    println!("A reopened:       A={}", r3.0);
    a.close().await;
    a2.close().await;
    if let Ok(root) = g.item_by_path(drive, prefix).await {
        g.delete_item(drive, &root.id).await?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn seed(g: &GraphClient, drive: &str, prefix: &str, files: usize, large: usize, large_mb: usize, fanout: usize, parallel: usize) -> Result<()> {
    // Folder tree: prefix/dN/dM (two levels), plus a few awkward names.
    let mut folders = vec![prefix.to_string()];
    for i in 0..fanout {
        let a = format!("{prefix}/dir {i}");
        folders.push(a.clone());
        for j in 0..fanout.min(3) {
            folders.push(format!("{a}/sub-{j}"));
        }
    }
    folders.push(format!("{prefix}/Ünïcödé & spaces #1"));
    for f in &folders {
        g.ensure_folder(drive, f).await.with_context(|| format!("creating {f}"))?;
    }
    println!("created {} folders", folders.len());

    let mut rng = Rng::seeded();
    let mut jobs: Vec<(String, Bytes)> = vec![];
    for i in 0..files {
        let dir = &folders[rng.below(folders.len())];
        let size = [0, 1, 100, 1000, 10_000, 70_000][rng.below(6)] + rng.below(500);
        let ext = ["txt", "md", "docx", "bin", "json"][rng.below(5)];
        jobs.push((format!("{dir}/file-{i:04}.{ext}"), Bytes::from(rng.bytes(size))));
    }
    for i in 0..large {
        let size = large_mb * 1024 * 1024 + rng.below(1000);
        jobs.push((format!("{prefix}/large-{i}.bin"), Bytes::from(rng.bytes(size))));
    }
    let total = jobs.len();
    let mut done = 0;
    let mut stream = futures_util::stream::iter(jobs)
        .map(|(path, data)| async move {
            let r = g.upload(drive, &path, data).await;
            (path, r)
        })
        .buffer_unordered(parallel);
    while let Some((path, r)) = stream.next().await {
        r.with_context(|| format!("uploading {path}"))?;
        done += 1;
        if done % 50 == 0 || done == total {
            println!("uploaded {done}/{total}");
        }
    }
    Ok(())
}

/// Every item under `prefix`, as (path relative to library root, item).
async fn list(g: &GraphClient, drive: &str, prefix: &str) -> Result<Vec<(String, DriveItem)>> {
    let root = g.item_by_path(drive, prefix).await?;
    let mut out = vec![];
    let mut stack = vec![(prefix.to_string(), root.id.clone())];
    while let Some((path, id)) = stack.pop() {
        for c in g.children(drive, &id).await? {
            let p = format!("{path}/{}", c.name.clone().unwrap_or_default());
            if c.folder.is_some() {
                stack.push((p.clone(), c.id.clone()));
            }
            out.push((p, c));
        }
    }
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
async fn mutate(g: &GraphClient, drive: &str, prefix: &str, edits: usize, deletes: usize, renames: usize, folder_renames: usize, creates: usize) -> Result<()> {
    let all = list(g, drive, prefix).await?;
    let mut files: Vec<_> = all.iter().filter(|(_, i)| i.folder.is_none()).cloned().collect();
    let folders: Vec<_> = all.iter().filter(|(_, i)| i.folder.is_some()).cloned().collect();
    let mut rng = Rng::seeded();
    let pick = |v: &mut Vec<(String, DriveItem)>, rng: &mut Rng| -> Option<(String, DriveItem)> {
        if v.is_empty() { None } else { Some(v.swap_remove(rng.below(v.len()))) }
    };
    for _ in 0..edits {
        if let Some((p, _)) = pick(&mut files, &mut rng) {
            let size = 10 + rng.below(5000);
            g.upload(drive, &p, Bytes::from(rng.bytes(size))).await?;
            println!("edit    {p}");
        }
    }
    for _ in 0..deletes {
        if let Some((p, it)) = pick(&mut files, &mut rng) {
            g.delete_item(drive, &it.id).await?;
            println!("delete  {p}");
        }
    }
    for _ in 0..renames {
        if let Some((p, it)) = pick(&mut files, &mut rng) {
            let name = format!("renamed-{}-{}", rng.below(100_000), it.name.clone().unwrap_or_default());
            g.move_item(drive, &it.id, None, Some(&name)).await?;
            println!("rename  {p} -> {name}");
        }
    }
    let mut folders = folders;
    for _ in 0..folder_renames {
        if let Some((p, it)) = pick(&mut folders, &mut rng) {
            let name = format!("{}-renamed{}", it.name.clone().unwrap_or_default(), rng.below(1000));
            g.move_item(drive, &it.id, None, Some(&name)).await?;
            println!("rendir  {p} -> {name}");
            // Its subtree paths changed; don't touch them again in this run.
            files.retain(|(fp, _)| !fp.starts_with(&format!("{p}/")));
            folders.retain(|(fp, _)| !fp.starts_with(&format!("{p}/")));
        }
    }
    for i in 0..creates {
        let p = format!("{prefix}/new-{}-{i}.txt", rng.below(1_000_000));
        let size = 100 + rng.below(1000);
        g.upload(drive, &p, Bytes::from(rng.bytes(size))).await?;
        println!("create  {p}");
    }
    Ok(())
}


/// M3 probe: raw Engine.IO / socket.io frames from Graph's push endpoint.
async fn push_probe(g: &GraphClient, drive: &str, eio: u8, namespace: bool, secs: u64, mutate_every: u64, prefix: &str) -> Result<()> {
    use futures_util::{SinkExt, StreamExt};
    use std::time::{Duration, Instant};
    use tokio_tungstenite::tungstenite::Message;

    let sub = g.socket_io_endpoint(drive).await?;
    let url = url::Url::parse(&sub.notification_url)?;
    eprintln!("subscription id={:?} expires={:?}", sub.id, sub.expiration_date_time);
    eprintln!("endpoint host={} path={} query_keys={:?}", url.host_str().unwrap_or(""), url.path(),
        url.query_pairs().map(|(k, _)| k.into_owned()).collect::<Vec<_>>());
    // The docs: the part before `/callback?` is the endpoint (its path being the namespace), the rest is opaque query.
    let ns = url.path().trim_end_matches("/callback").to_string();
    let mut ws_url = url.clone();
    ws_url.set_scheme("wss").ok();
    if namespace {
        ws_url.set_path("/socket.io/");
    } else {
        ws_url.set_path(&format!("{ns}/socket.io/"));
    }
    let q = format!("{}&EIO={eio}&transport=websocket", url.query().unwrap_or(""));
    ws_url.set_query(Some(&q));
    eprintln!("connecting to wss://{}{} (namespace {})", ws_url.host_str().unwrap_or(""), ws_url.path(), if namespace { &ns } else { "/" });

    let t0 = Instant::now();
    let (mut ws, resp) = tokio_tungstenite::connect_async(ws_url.as_str()).await?;
    eprintln!("[{:>7.2}s] websocket open: HTTP {}", t0.elapsed().as_secs_f64(), resp.status());

    let mut delta = g.delta_latest(drive).await?;
    let mut writes: Vec<Instant> = vec![];
    let mut n = 0u32;
    let mut mutate = tokio::time::interval(Duration::from_secs(mutate_every.max(1)));
    mutate.tick().await;
    let mut ping3 = tokio::time::interval(Duration::from_secs(25));
    ping3.tick().await;
    let deadline = tokio::time::sleep(Duration::from_secs(secs));
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            _ = &mut deadline => break,
            _ = mutate.tick(), if mutate_every > 0 => {
                n += 1;
                g.upload(drive, &format!("{prefix}/p{n:03}.txt"), bytes::Bytes::from(format!("probe {n}\n"))).await?;
                writes.push(Instant::now());
                eprintln!("[{:>7.2}s] wrote {prefix}/p{n:03}.txt", t0.elapsed().as_secs_f64());
            }
            _ = ping3.tick(), if eio == 3 => { ws.send(Message::text("2")).await?; }
            msg = ws.next() => {
                let Some(msg) = msg else { eprintln!("[{:>7.2}s] stream ended", t0.elapsed().as_secs_f64()); break };
                let msg = match msg { Ok(m) => m, Err(e) => { eprintln!("[{:>7.2}s] error: {e}", t0.elapsed().as_secs_f64()); break } };
                let at = t0.elapsed().as_secs_f64();
                match msg {
                    Message::Text(t) => {
                        let t = t.as_str();
                        eprintln!("[{at:>7.2}s] < {}", if t.len() > 600 { &t[..600] } else { t });
                        if t.starts_with('0') && namespace {
                            let join = format!("40{ns},");
                            eprintln!("[{at:>7.2}s] > {join}");
                            ws.send(Message::text(join)).await?;
                        } else if t == "2" && eio == 4 {
                            ws.send(Message::text("3")).await?;
                        } else if t.starts_with("42") {
                            if let Some(w) = writes.last() {
                                eprintln!("           notification {:.2}s after the last write", w.elapsed().as_secs_f64());
                            }
                            // Is the change visible in delta yet?
                            let page = g.delta_page(drive, Some(&delta)).await?;
                            let names: Vec<_> = page.value.iter().filter_map(|i| i.name.clone()).collect();
                            eprintln!("           delta right after: {} item(s) {:?}", names.len(), names);
                            if let Some(l) = page.delta_link { delta = l; }
                        }
                    }
                    Message::Close(c) => { eprintln!("[{at:>7.2}s] close: {c:?}"); break }
                    other => eprintln!("[{at:>7.2}s] < {other:?}"),
                }
            }
        }
    }
    eprintln!("probe ended after {:.0}s", t0.elapsed().as_secs_f64());
    if mutate_every > 0 && let Ok(root) = g.item_by_path(drive, prefix).await {
        g.delete_item(drive, &root.id).await?;
    }
    Ok(())
}
