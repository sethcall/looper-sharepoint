//! Live end-to-end test against the QA tenant (see `docs/admin-setup.md`).
//!
//!     cargo test -p looper-sharepoint --features live -- --test-threads=1
//!
//! Reads TENANT_ID, APP_CLIENT_ID, QA_USER_1, QA_USER_1_PASSWORD, QA_SITE_URL, QA_DRIVE_ID from the environment or
//! the repo's `.env`. Works in its own folder `runs/live-<random>/` and deletes it (to the recycle bin) at the end.
#![cfg(feature = "live")]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use bytes::Bytes;
use serde_json::Value;
use looper_sharepoint_engine::auth::{Authority, PasswordProvider};
use looper_sharepoint_engine::graph::GraphClient;

fn env() -> HashMap<String, String> {
    let mut vars = HashMap::new();
    let dotenv = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.env");
    if let Ok(text) = std::fs::read_to_string(dotenv) {
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

struct Harness {
    vars: HashMap<String, String>,
    dir: tempfile::TempDir,
    prefix: String,
    graph: GraphClient,
    drive: String,
    /// The local folder that `prefix` in SharePoint corresponds to.
    base: PathBuf,
}

impl Harness {
    fn new() -> Self {
        Self::with("", "")
    }

    /// `sync_extra` goes into `[sync]`, `lib_extra` into the `[[library]]` block.
    fn with(sync_extra: &str, lib_extra: &str) -> Self {
        Self::configured(|prefix, dir, site| {
            let h = format!("[sync]\nroot = \"{}\"\n{sync_extra}\n", dir.join("root").display());
            let lib = format!(
                "[[library]]\nsite = \"{site}\"\nlibrary = \"Documents\"\nlocal = \"qa\"\ninclude = [\"{prefix}/**\"]\n{lib_extra}\n"
            );
            (format!("{h}{lib}"), dir.join("root/qa").join(prefix))
        })
    }

    /// `libs(prefix, temp dir, site URL)` gives the config after `[account]`, and the local folder for `prefix`.
    fn configured(libs: impl FnOnce(&str, &Path, &str) -> (String, PathBuf)) -> Self {
        let vars = env();
        let get = |k: &str| vars.get(k).unwrap_or_else(|| panic!("{k} not set")).clone();
        let mut r = [0u8; 4];
        pseudo_random(&mut r);
        let prefix = format!("runs/live-{:08x}", u32::from_le_bytes(r));
        let dir = tempfile::tempdir().unwrap();
        let (rest, base) = libs(&prefix, dir.path(), &get("QA_SITE_URL"));
        let config = format!("[account]\ntenant = \"{}\"\nclient_id = \"{}\"\n{rest}", get("TENANT_ID"), get("APP_CLIENT_ID"));
        std::fs::write(dir.path().join("config.toml"), config).unwrap();
        let auth = Authority {
            tenant: get("TENANT_ID"),
            client_id: get("APP_CLIENT_ID"),
            scopes: vec!["https://graph.microsoft.com/.default".into()],
        };
        let graph = GraphClient::new(PasswordProvider::new(auth, get("QA_USER_1"), get("QA_USER_1_PASSWORD")));
        let drive = get("QA_DRIVE_ID");
        Self { vars, dir, prefix, graph, drive, base }
    }

    fn local(&self, rel: &str) -> PathBuf {
        self.base.join(rel)
    }

    /// Run `looper-sharepoint sync --json`; returns (exit code, report for the library).
    fn sync(&self, extra: &[&str]) -> (i32, Value) {
        let (code, v) = self.sync_all(extra);
        (code, v[0].clone())
    }

    /// Run `looper-sharepoint sync --json`; returns (exit code, the report for every library).
    fn sync_all(&self, extra: &[&str]) -> (i32, Value) {
        let out = Command::new(env!("CARGO_BIN_EXE_looper-sharepoint"))
            .args(["--json", "sync"])
            .args(extra)
            .env("LOOPER_SHAREPOINT_CONFIG", self.dir.path().join("config.toml"))
            .env("LOOPER_SHAREPOINT_DATA_DIR", self.dir.path().join("data"))
            .env("LOOPER_SHAREPOINT_TEST_USERNAME", &self.vars["QA_USER_1"])
            .env("LOOPER_SHAREPOINT_TEST_PASSWORD", &self.vars["QA_USER_1_PASSWORD"])
            .env("RUST_LOG", "error")
            .output()
            .unwrap();
        let v: Value = serde_json::from_slice(&out.stdout)
            .unwrap_or_else(|e| panic!("bad json ({e}): {}\n{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)));
        (out.status.code().unwrap_or(-1), v["libraries"].clone())
    }

    async fn put(&self, rel: &str, content: &str) {
        self.graph.upload(&self.drive, &format!("{}/{rel}", self.prefix), Bytes::from(content.to_string())).await.unwrap();
    }

    async fn id(&self, rel: &str) -> String {
        self.graph.item_by_path(&self.drive, &format!("{}/{rel}", self.prefix)).await.unwrap().id
    }

    /// Does `rel` exist in SharePoint?
    async fn exists(&self, rel: &str) -> bool {
        self.graph.item_by_path(&self.drive, &format!("{}/{rel}", self.prefix)).await.is_ok()
    }

    /// Content of `rel` in SharePoint.
    async fn remote_text(&self, rel: &str) -> String {
        use futures_util::TryStreamExt;
        let id = self.id(rel).await;
        let stream = self.graph.download(&self.drive, &id).await.unwrap();
        let chunks: Vec<Bytes> = stream.try_collect().await.unwrap();
        String::from_utf8_lossy(&chunks.concat()).into_owned()
    }

    async fn cleanup(&self) {
        if let Ok(root) = self.graph.item_by_path(&self.drive, &self.prefix).await {
            self.graph.delete_item(&self.drive, &root.id).await.unwrap();
        }
    }
}

fn write(p: &Path, content: &str) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, content).unwrap();
}

fn n(v: &Value) -> usize {
    match v {
        Value::Array(a) => a.len(),
        v => v.as_u64().unwrap_or(0) as usize,
    }
}

fn pseudo_random(b: &mut [u8]) {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    for (i, x) in b.iter_mut().enumerate() {
        *x = (nanos >> (i * 8)) as u8 ^ (std::process::id() >> (i % 4 * 8)) as u8;
    }
}

fn read(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap_or_else(|e| format!("<{e}>"))
}

#[tokio::test]
async fn download_only_end_to_end() {
    let h = Harness::with("direction = \"download_only\"", "");

    // Initial sync.
    h.put("a.txt", "a1").await;
    h.put("dir/b.txt", "b1").await;
    h.put("dir/sub/c.txt", "c1").await;
    h.put("gone.txt", "g1").await;
    let (code, r) = h.sync(&[]);
    assert_eq!(code, 0, "{r}");
    assert_eq!(r["downloaded"], 4, "{r}");
    assert_eq!(read(&h.local("dir/sub/c.txt")), "c1");

    // Idempotent.
    let (_, r) = h.sync(&[]);
    assert_eq!(r["downloaded"], 0, "{r}");

    // Remote edit, delete, folder rename.
    h.put("a.txt", "a2").await;
    let gone = h.id("gone.txt").await;
    h.graph.delete_item(&h.drive, &gone).await.unwrap();
    let dir = h.id("dir").await;
    h.graph.move_item(&h.drive, &dir, None, Some("dir2")).await.unwrap();
    let (code, r) = h.sync(&[]);
    assert_eq!(code, 0, "{r}");
    assert_eq!(read(&h.local("a.txt")), "a2");
    assert!(!h.local("gone.txt").exists(), "remote delete removes local copy");
    assert_eq!(read(&h.local("dir2/sub/c.txt")), "c1", "folder rename moves subtree");
    assert!(!h.local("dir").exists());
    assert_eq!(r["downloaded"], 1, "folder rename must not re-download children: {r}");

    // Local edit + remote edit: conflict, local kept. Local delete: restored. Orphan: kept.
    std::fs::write(h.local("a.txt"), "mine").unwrap();
    h.put("a.txt", "a3").await;
    std::fs::remove_file(h.local("dir2/b.txt")).unwrap();
    std::fs::write(h.local("orphan.txt"), "local only").unwrap();
    let (code, r) = h.sync(&[]);
    assert_eq!(code, 1, "conflicts give exit code 1: {r}");
    assert_eq!(read(&h.local("a.txt")), "mine", "never overwrite local edits");
    assert_eq!(read(&h.local("dir2/b.txt")), "b1", "locally deleted file restored");
    assert_eq!(read(&h.local("orphan.txt")), "local only");
    assert_eq!(r["conflicts"].as_array().unwrap().len(), 1, "{r}");

    // Cleanup.
    let root = h.graph.item_by_path(&h.drive, &h.prefix).await.unwrap();
    h.graph.delete_item(&h.drive, &root.id).await.unwrap();
}

/// Two-way sync, spec `specs/m4-design.md` §9 (live 1–8, 11, 12).
#[tokio::test]
async fn two_way_end_to_end() {
    let h = Harness::new();
    h.put("a.txt", "a1").await;
    h.put("dir/b.txt", "b1").await;
    h.put("dir/sub/c.txt", "c1").await;
    h.put("del.txt", "d1").await;
    let (code, r) = h.sync(&[]);
    assert_eq!(code, 0, "{r}");
    assert_eq!(r["downloaded"], 4, "{r}");
    let a_id = h.id("a.txt").await;
    let c_id = h.id("dir/sub/c.txt").await;

    // 1, 2, 11: new file, new folder, in-place edit, empty file, large (chunked) file.
    write(&h.local("new.txt"), "new1");
    write(&h.local("newdir/x.txt"), "x1");
    write(&h.local("a.txt"), "a-local");
    write(&h.local("empty.txt"), "");
    let big: String = (0..25 * 1024 * 1024 / 16).map(|i| format!("{i:015}\n")).collect();
    write(&h.local("big.txt"), &big);
    let (code, r) = h.sync(&["--dry-run"]);
    assert_eq!(code, 0, "{r}");
    assert_eq!(n(&r["uploaded"]), 5, "dry run predicts the uploads: {r}");
    assert!(!h.exists("new.txt").await, "dry run changes nothing");
    let (code, r) = h.sync(&[]);
    assert_eq!(code, 0, "{r}");
    assert_eq!(r["uploaded"], 5, "{r}");
    assert_eq!(r["created_remote"], 1, "{r}");
    assert_eq!(h.remote_text("new.txt").await, "new1");
    assert_eq!(h.remote_text("newdir/x.txt").await, "x1");
    assert_eq!(h.remote_text("a.txt").await, "a-local");
    assert_eq!(h.id("a.txt").await, a_id, "edits upload in place (same item, version history kept)");
    let big_item = h.graph.item_by_path(&h.drive, &format!("{}/big.txt", h.prefix)).await.unwrap();
    assert_eq!(big_item.size, Some(big.len() as i64));
    let local_hash = looper_sharepoint_engine::core::quickxor::hash_file(&h.local("big.txt")).unwrap();
    assert_eq!(big_item.quickxor(), Some(local_hash.as_str()), "chunked upload matches");
    assert!(h.exists("empty.txt").await);

    // 12: echo. Our own uploads come back through delta and must not download or conflict.
    let (code, r) = h.sync(&[]);
    assert_eq!(code, 0, "{r}");
    assert_eq!((n(&r["downloaded"]), n(&r["uploaded"]), n(&r["conflicts"])), (0, 0, 0), "{r}");

    // 4: local renames/moves: a file, and a whole folder (one PATCH, children keep their ids, nothing re-uploaded).
    std::fs::rename(h.local("a.txt"), h.local("a-renamed.txt")).unwrap();
    std::fs::rename(h.local("dir"), h.local("dir-renamed")).unwrap();
    let (code, r) = h.sync(&["--dry-run"]);
    assert_eq!(code, 0, "{r}");
    assert_eq!((n(&r["moved_remote"]), n(&r["uploaded"]), n(&r["deleted_remote"])), (2, 0, 0), "dry run reports renames as renames: {r}");
    assert!(h.exists("a.txt").await, "dry run changes nothing");
    let (code, r) = h.sync(&[]);
    assert_eq!(code, 0, "{r}");
    assert_eq!(r["moved_remote"], 2, "{r}");
    assert_eq!(r["uploaded"], 0, "renames must not re-upload: {r}");
    assert_eq!(n(&r["deleted_remote"]), 0, "{r}");
    assert_eq!(h.id("a-renamed.txt").await, a_id);
    assert_eq!(h.id("dir-renamed/sub/c.txt").await, c_id);
    assert!(!h.exists("dir").await);

    // 3: local delete → recycle bin.
    std::fs::remove_file(h.local("del.txt")).unwrap();
    let (_, r) = h.sync(&["--dry-run"]);
    assert_eq!(r["deleted_remote"], serde_json::json!([format!("{}/del.txt", h.prefix)]), "{r}");
    let (code, r) = h.sync(&[]);
    assert_eq!(code, 0, "{r}");
    assert_eq!(r["deleted_remote"], serde_json::json!([format!("{}/del.txt", h.prefix)]), "{r}");
    assert!(!h.exists("del.txt").await);

    // 5: both sides edited → conflict copy on both sides, remote version at the original path.
    write(&h.local("new.txt"), "mine");
    h.put("new.txt", "theirs").await;
    // 6: deleted locally, edited remotely → restored.
    std::fs::remove_file(h.local("dir-renamed/b.txt")).unwrap();
    h.put("dir-renamed/b.txt", "b2").await;
    // 7: deleted remotely, edited locally → uploaded again.
    let x = h.id("newdir/x.txt").await;
    h.graph.delete_item(&h.drive, &x).await.unwrap();
    write(&h.local("newdir/x.txt"), "x-edited");
    let (code, r) = h.sync(&[]);
    assert_eq!(code, 1, "a conflict is reported: {r}");
    assert_eq!(n(&r["conflict_copies"]), 1, "{r}");
    let copy = r["conflict_copies"][0].as_str().unwrap().to_string();
    let copy_rel = copy.strip_prefix(&format!("{}/", h.prefix)).unwrap().to_string();
    assert_eq!(read(&h.local("new.txt")), "theirs");
    assert_eq!(read(&h.local(&copy_rel)), "mine");
    assert_eq!(h.remote_text(&copy_rel).await, "mine", "the conflict copy is uploaded too");
    assert_eq!(read(&h.local("dir-renamed/b.txt")), "b2", "local delete + remote edit restores");
    assert_eq!(h.remote_text("newdir/x.txt").await, "x-edited", "remote delete + local edit re-uploads");
    let (code, r) = h.sync(&[]);
    assert_eq!(code, 0, "settles after the conflict: {r}");

    // 8: local mass delete is refused, nothing deleted in SharePoint; allowed with the override.
    for i in 0..14 {
        write(&h.local(&format!("bulk/f{i:02}.txt")), &format!("bulk {i}"));
    }
    let (code, r) = h.sync(&[]);
    assert_eq!((code, n(&r["uploaded"])), (0, 14), "{r}");
    std::fs::remove_dir_all(h.local("bulk")).unwrap();
    let (code, r) = h.sync(&[]);
    assert_eq!(code, 1, "{r}");
    assert_eq!(r["outcome"], "refused", "{r}");
    assert!(h.exists("bulk/f00.txt").await, "refused run deletes nothing");
    let (code, r) = h.sync(&["--allow-mass-delete"]);
    assert_eq!(code, 0, "{r}");
    assert_eq!(r["deleted_remote"], serde_json::json!([format!("{}/bulk/", h.prefix)]), "whole folder deleted as one: {r}");
    assert!(!h.exists("bulk").await);

    // 9: the local library folder disappears → error, nothing deleted.
    let lib_root = h.dir.path().join("root/qa");
    let moved = h.dir.path().join("root/qa-elsewhere");
    std::fs::rename(&lib_root, &moved).unwrap();
    let (code, r) = h.sync(&[]);
    assert_eq!(code, 1, "{r}");
    assert!(r["error"].as_str().unwrap_or("").contains("missing"), "{r}");
    assert!(h.exists("new.txt").await, "missing local folder deletes nothing");
    std::fs::rename(&moved, &lib_root).unwrap();

    h.cleanup().await;
}

/// 10: read-only libraries never upload.
#[tokio::test]
async fn read_only_never_uploads() {
    let h = Harness::with("", "read_only = true");
    h.put("a.txt", "a1").await;
    let (code, r) = h.sync(&[]);
    assert_eq!(code, 0, "{r}");
    write(&h.local("a.txt"), "edited");
    write(&h.local("new.txt"), "new");
    std::fs::remove_file(h.local("a.txt")).ok();
    write(&h.local("a.txt"), "edited");
    let (code, r) = h.sync(&[]);
    assert_eq!(code, 0, "{r}");
    assert_eq!(n(&r["uploaded"]), 0, "{r}");
    assert_eq!(h.remote_text("a.txt").await, "a1");
    assert!(!h.exists("new.txt").await);
    h.cleanup().await;
}

/// A folder deleted locally while someone adds a file to it in SharePoint: the folder (and their file) stays; only
/// the files we knew are deleted.
#[tokio::test]
async fn folder_delete_keeps_files_added_remotely() {
    let h = Harness::new();
    h.put("keep/a.txt", "a").await;
    h.put("keep/sub/b.txt", "b").await;
    let (code, r) = h.sync(&[]);
    assert_eq!(code, 0, "{r}");
    std::fs::remove_dir_all(h.local("keep")).unwrap();
    h.put("keep/sub/theirs.txt", "added in SharePoint").await;
    let (code, r) = h.sync(&[]);
    assert_eq!(code, 0, "{r}");
    assert!(h.exists("keep/sub/theirs.txt").await, "their new file survives: {r}");
    assert!(!h.exists("keep/a.txt").await && !h.exists("keep/sub/b.txt").await, "known files deleted: {r}");
    assert_eq!(read(&h.local("keep/sub/theirs.txt")), "added in SharePoint");
    h.cleanup().await;
}

/// A folder vanishes from SharePoint (deleted, or the user lost access: delta reports both the same way) while it has
/// local edits and new local files: they're moved out to `not-uploaded/`, and the folder is not recreated.
/// `specs/access-control-design.md` §6.
#[tokio::test]
async fn vanished_folder_sets_local_changes_aside() {
    let h = Harness::new();
    h.put("secret/a.txt", "a1").await;
    h.put("secret/sub/b.txt", "b1").await;
    h.put("open/c.txt", "c1").await;
    let (code, r) = h.sync(&[]);
    assert_eq!(code, 0, "{r}");
    write(&h.local("secret/a.txt"), "a-edited");
    write(&h.local("secret/sub/new.txt"), "new here");
    let secret = h.id("secret").await;
    h.graph.delete_item(&h.drive, &secret).await.unwrap();

    let (code, r) = h.sync(&["--dry-run"]);
    assert_eq!(code, 1, "{r}");
    assert!(h.local("secret/a.txt").exists(), "dry run moves nothing");

    let (code, r) = h.sync(&[]);
    assert_eq!(code, 1, "set-aside files need attention: {r}");
    assert_eq!(n(&r["uploaded"]), 0, "nothing uploaded: {r}");
    assert!(!h.exists("secret").await, "the folder is not recreated: {r}");
    assert!(!h.local("secret").exists(), "the local folder is gone: {r}");
    let aside = h.dir.path().join("root/not-uploaded/qa");
    let stamp = std::fs::read_dir(&aside).unwrap().next().unwrap().unwrap().path();
    assert_eq!(read(&stamp.join(&h.prefix).join("secret/a.txt")), "a-edited");
    assert_eq!(read(&stamp.join(&h.prefix).join("secret/sub/new.txt")), "new here");
    assert!(!stamp.join(&h.prefix).join("secret/sub/b.txt").exists(), "unedited files are just deleted");
    assert_eq!(read(&h.local("open/c.txt")), "c1");

    let (code, r) = h.sync(&[]);
    assert_eq!(code, 0, "settles: {r}");
    assert!(!h.exists("secret").await);
    h.cleanup().await;
}

/// A library limited to one SharePoint folder, synced directly into a folder of its own: `specs/onboarding-design.md`
/// §3.1. A second library syncs another folder of the same SharePoint library.
#[tokio::test]
async fn folder_as_library_root() {
    let h = Harness::configured(|prefix, dir, site| {
        let libs = format!(
            "[[library]]\nsite = \"{site}\"\nlibrary = \"Documents\"\nfolder = \"{prefix}/proj\"\npath = \"{}\"\n\
             [[library]]\nsite = \"{site}\"\nlibrary = \"Documents\"\nfolder = \"{prefix}/second\"\npath = \"{}\"\n",
            dir.join("anywhere/specs").display(),
            dir.join("elsewhere/second").display(),
        );
        (libs, dir.join("anywhere/specs"))
    });
    h.put("proj/a.txt", "a1").await;
    h.put("proj/sub/b.txt", "b1").await;
    h.put("outside/x.txt", "x1").await;
    h.put("outside/moving/m.txt", "m1").await;
    h.put("second/s.txt", "s1").await;
    h.put("top.txt", "t1").await;

    // Only the folder's contents, straight into the chosen folder; the other library has its own.
    let (code, all) = h.sync_all(&[]);
    assert_eq!(code, 0, "{all}");
    assert_eq!((n(&all[0]["downloaded"]), n(&all[1]["downloaded"])), (2, 1), "{all}");
    assert_eq!(read(&h.local("a.txt")), "a1");
    assert_eq!(read(&h.local("sub/b.txt")), "b1");
    assert_eq!(read(&h.dir.path().join("elsewhere/second/s.txt")), "s1");
    assert!(!h.local("proj").exists() && !h.local("x.txt").exists() && !h.local("top.txt").exists());

    // Uploads go into the folder.
    write(&h.local("new.txt"), "new1");
    write(&h.local("newdir/n.txt"), "n1");
    let (code, r) = h.sync(&[]);
    assert_eq!((code, n(&r["uploaded"])), (0, 2), "{r}");
    assert_eq!(h.remote_text("proj/new.txt").await, "new1");
    assert_eq!(h.remote_text("proj/newdir/n.txt").await, "n1");

    // Moved in from outside (a file, and a folder whose contents delta doesn't list again); moved out.
    let proj = h.id("proj").await;
    let x = h.id("outside/x.txt").await;
    h.graph.move_item(&h.drive, &x, Some(&proj), None).await.unwrap();
    let moving = h.id("outside/moving").await;
    h.graph.move_item(&h.drive, &moving, Some(&proj), None).await.unwrap();
    let outside = h.id("outside").await;
    let sub = h.id("proj/sub").await;
    h.graph.move_item(&h.drive, &sub, Some(&outside), None).await.unwrap();
    let (code, r) = h.sync(&[]);
    assert_eq!(code, 0, "{r}");
    assert_eq!(read(&h.local("x.txt")), "x1", "{r}");
    assert_eq!(read(&h.local("moving/m.txt")), "m1", "a folder moved in brings its contents: {r}");
    assert!(!h.local("sub").exists(), "moved out is gone locally: {r}");
    assert!(h.exists("outside/sub/b.txt").await, "and stays in SharePoint where it went");

    // The folder itself renamed and moved in SharePoint: followed by id, nothing changes locally.
    h.graph.move_item(&h.drive, &proj, Some(&outside), Some("proj-renamed")).await.unwrap();
    h.put("outside/proj-renamed/after.txt", "after").await;
    let (code, r) = h.sync(&[]);
    assert_eq!(code, 0, "{r}");
    assert_eq!(n(&r["deleted_local"]), 0, "{r}");
    assert_eq!(read(&h.local("after.txt")), "after", "{r}");
    assert_eq!(read(&h.local("a.txt")), "a1");

    // The folder deleted: an error, and nothing local deleted.
    let renamed = h.id("outside/proj-renamed").await;
    h.graph.delete_item(&h.drive, &renamed).await.unwrap();
    let (code, all) = h.sync_all(&[]);
    assert_eq!(code, 1, "{all}");
    assert!(all[0]["error"].as_str().unwrap_or("").contains("is gone"), "{all}");
    assert_eq!(read(&h.local("a.txt")), "a1", "nothing deleted locally");
    assert_eq!(read(&h.local("moving/m.txt")), "m1");

    h.cleanup().await;
}

impl Harness {
    /// Run `looper-sharepoint --json ARGS`; returns (exit code, stdout as JSON or a string, stderr).
    fn cli(&self, args: &[&str]) -> (i32, Value, String) {
        let out = Command::new(env!("CARGO_BIN_EXE_looper-sharepoint"))
            .arg("--json")
            .args(args)
            .env("LOOPER_SHAREPOINT_CONFIG", self.dir.path().join("config.toml"))
            .env("LOOPER_SHAREPOINT_DATA_DIR", self.dir.path().join("data"))
            .env("LOOPER_SHAREPOINT_TEST_USERNAME", &self.vars["QA_USER_1"])
            .env("LOOPER_SHAREPOINT_TEST_PASSWORD", &self.vars["QA_USER_1_PASSWORD"])
            .env("RUST_LOG", "error")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        // `add` prints its own line, then the first sync's report; other commands one (pretty) document.
        let v = serde_json::from_str::<Value>(&stdout).unwrap_or_else(|_| stdout.lines().filter_map(|l| serde_json::from_str::<Value>(l).ok()).fold(json!({}), |mut acc, v| {
            if let (Some(a), Some(o)) = (acc.as_object_mut(), v.as_object()) {
                a.extend(o.clone());
            }
            acc
        }));
        (out.status.code().unwrap_or(-1), v, String::from_utf8_lossy(&out.stderr).into_owned())
    }

    fn config_text(&self) -> String {
        read(&self.dir.path().join("config.toml"))
    }

    /// The browser's address for a folder (or library) in the QA library.
    fn web(&self, rel: &str) -> String {
        let site = self.vars["QA_SITE_URL"].trim_end_matches('/');
        let enc = |s: &str| s.split('/').map(|seg| seg.replace(' ', "%20")).collect::<Vec<_>>().join("/");
        format!("{site}/Shared%20Documents/{}", enc(rel))
    }
}

use serde_json::json;

/// `add` from each kind of link, `remove`, and the checks in between: `specs/onboarding-design.md` §5.
#[tokio::test]
async fn add_and_remove_from_links() {
    let h = Harness::configured(|_, dir, _| (format!("[sync]\nroot = \"{}\"\n", dir.join("root").display()), dir.join("unused")));
    let p = h.prefix.clone();
    h.put("plain/a.txt", "a").await;
    h.put("browser/b.txt", "b").await;
    h.put("shared/Deep Folder/c.txt", "c").await;

    // A site link (the whole library): resolved to the site's main library. Not synced here (--no-sync).
    let (code, v, err) = h.cli(&["add", &h.vars["QA_SITE_URL"], "--to", h.dir.path().join("work/all").to_str().unwrap(), "--no-sync"]);
    assert_eq!(code, 0, "{v} {err}");
    assert_eq!((v["library"].as_str(), v["folder"].as_str()), (Some("Documents"), None), "{v}");
    // A folder in it would overlap: refused.
    let (code, _, err) = h.cli(&["add", &h.web(&format!("{p}/plain")), "--to", h.dir.path().join("work/x").to_str().unwrap()]);
    assert!(code != 0 && err.contains("same SharePoint folder"), "{err}");
    let (code, _, err) = h.cli(&["remove", "all"]);
    assert_eq!(code, 0, "{err}");

    // A plain folder link, into a folder of our choosing.
    let to = h.dir.path().join("work/plain");
    let (code, v, err) = h.cli(&["add", &h.web(&format!("{p}/plain")), "--to", to.to_str().unwrap()]);
    assert_eq!(code, 0, "{v} {err}");
    assert_eq!((v["added"].as_bool(), v["name"].as_str(), v["folder"].as_str()), (Some(true), Some("plain"), Some(&*format!("{p}/plain"))), "{v}");
    assert_eq!(read(&to.join("a.txt")), "a", "first sync done by add: {v} {err}");

    // The browser's AllItems page for a folder; the suggested folder (no --to) under sync.root.
    let site_path = url::Url::parse(&h.vars["QA_SITE_URL"]).unwrap().path().trim_end_matches('/').to_string();
    let id = format!("{site_path}/Shared Documents/{p}/browser");
    let q: String = url::form_urlencoded::byte_serialize(id.as_bytes()).collect();
    let all_items = format!("{}/Shared%20Documents/Forms/AllItems.aspx?id={}&viewid=00000000-0000-0000-0000-000000000000", h.vars["QA_SITE_URL"].trim_end_matches('/'), q.replace('+', "%20"));
    let (code, v, err) = h.cli(&["add", &all_items, "--yes"]);
    assert_eq!(code, 0, "{v} {err}");
    assert_eq!(read(&h.dir.path().join("root/browser/b.txt")), "b", "{v} {err}");

    // A sharing link to a folder with a space in its name.
    let deep = h.id("shared/Deep Folder").await;
    let resp = h.graph.send(reqwest::Method::POST, &format!("/drives/{}/items/{deep}/createLink", h.drive), |r| {
        r.header("Content-Type", "application/json").body(r#"{"type":"view","scope":"organization"}"#)
    }).await.unwrap();
    let link: Value = resp.json().await.unwrap();
    let link = link["link"]["webUrl"].as_str().unwrap().to_string();
    let deep_to = h.dir.path().join("work/deep");
    let (code, v, err) = h.cli(&["add", &link, "--to", deep_to.to_str().unwrap(), "--read-only"]);
    assert_eq!(code, 0, "{v} {err}");
    assert_eq!(v["name"], "deep", "{v}");
    assert_eq!(read(&deep_to.join("c.txt")), "c", "{v} {err}");
    assert!(h.config_text().contains("read_only = true"));

    // The same folder again: reported, nothing added.
    let (code, v, _) = h.cli(&["add", &h.web(&format!("{p}/plain")), "--to", h.dir.path().join("work/other").to_str().unwrap()]);
    assert_eq!((code, v["added"].as_bool(), v["already"].as_str()), (0, Some(false), Some("plain")), "{v}");

    // Overlapping: inside another library's folder.
    let (code, _, err) = h.cli(&["add", &h.web(&format!("{p}/shared")), "--to", to.join("inner").to_str().unwrap()]);
    assert_ne!(code, 0);
    assert!(err.contains("inside"), "{err}");
    // A file link is refused; so is a non-empty folder, unless --adopt.
    let (code, _, err) = h.cli(&["add", &h.web(&format!("{p}/plain/a.txt")), "--to", h.dir.path().join("work/file").to_str().unwrap()]);
    assert!(code != 0 && err.contains("file"), "{err}");
    let busy = h.dir.path().join("work/busy");
    write(&busy.join("mine.txt"), "mine");
    let (code, _, err) = h.cli(&["add", &h.web(&format!("{p}/shared")), "--to", busy.to_str().unwrap()]);
    assert!(code != 0 && err.contains("isn't empty"), "{err}");
    let (code, v, err) = h.cli(&["add", &h.web(&format!("{p}/shared")), "--to", busy.to_str().unwrap(), "--adopt", "--name", "busy"]);
    assert_ne!(code, 0, "the read-only `deep` library overlaps `busy` in SharePoint: refused: {v} {err}");
    assert!(err.contains("same SharePoint folder"), "{err}");
    let (code, _, err) = h.cli(&["remove", "deep"]);
    assert_eq!(code, 0, "{err}");
    let (code, v, err) = h.cli(&["add", &h.web(&format!("{p}/shared")), "--to", busy.to_str().unwrap(), "--adopt", "--name", "busy"]);
    assert_eq!(code, 0, "{v} {err}");
    assert_eq!(h.remote_text("shared/mine.txt").await, "mine", "--adopt uploads files only here");
    assert_eq!(read(&busy.join("Deep Folder/c.txt")), "c");

    // remove: the files stay, the config no longer has it, and adding it again starts fresh.
    let (code, v, err) = h.cli(&["remove", "plain"]);
    assert_eq!(code, 0, "{v} {err}");
    assert_eq!(v["state_forgotten"], true, "{v}");
    assert_eq!(read(&to.join("a.txt")), "a");
    assert!(h.exists("plain/a.txt").await);
    assert!(!h.config_text().contains("/plain\""), "{}", h.config_text());

    h.cleanup().await;
}

/// A running host picks up `add` (and `remove`) at once, and does the first sync itself.
#[tokio::test]
async fn running_host_reloads_after_add() {
    let h = Harness::configured(|_, dir, _| (format!("[sync]\nroot = \"{}\"\npush = false\n", dir.join("root").display()), dir.join("unused")));
    h.put("live/a.txt", "a").await;
    let mut host = Command::new(env!("CARGO_BIN_EXE_looper-sharepoint"))
        .args(["run", "--no-tui"])
        .env("LOOPER_SHAREPOINT_CONFIG", h.dir.path().join("config.toml"))
        .env("LOOPER_SHAREPOINT_DATA_DIR", h.dir.path().join("data"))
        .env("LOOPER_SHAREPOINT_TEST_USERNAME", &h.vars["QA_USER_1"])
        .env("LOOPER_SHAREPOINT_TEST_PASSWORD", &h.vars["QA_USER_1_PASSWORD"])
        .env("RUST_LOG", "error")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    // Wait for its control socket.
    for _ in 0..50 {
        if h.cli(&["status"]).1["live"] == true {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    let to = h.dir.path().join("work/live");
    let (code, v, err) = h.cli(&["add", &h.web(&format!("{}/live", h.prefix)), "--to", to.to_str().unwrap()]);
    let (_, status, _) = h.cli(&["status"]);
    let (rm, _, rm_err) = h.cli(&["remove", "live"]);
    let (_, after, _) = h.cli(&["status"]);
    let _ = host.kill();
    let _ = host.wait();
    assert_eq!(code, 0, "{v} {err}");
    assert_eq!(read(&to.join("a.txt")), "a");
    assert_eq!((status["live"].as_bool(), status["status"]["libraries"][0]["name"].as_str()), (Some(true), Some("live")), "{status}");
    assert_eq!(rm, 0, "{rm_err}");
    assert_eq!(after["status"]["libraries"].as_array().map(Vec::len), Some(0), "{after}");
    h.cleanup().await;
}
