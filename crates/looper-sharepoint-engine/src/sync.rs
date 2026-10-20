//! Sync of one library: one run, start to finish. Download-only per `specs/m1-design.md` §3; two-way per
//! `specs/m4-design.md` (the local → SharePoint half lives in [`two_way`]). The long-running scheduler that calls this
//! lives in [`crate::service`].
//!
//! Each run:
//! 1. Pull delta (from the saved deltaLink, or a full enumeration) and stage remote state into the DB.
//! 2. Two-way: refuse if the local folder is missing; apply local renames/moves in SharePoint first.
//! 3. Work out every item's desired local path (name mapping, filters, collisions).
//! 4. Plan deletes in both directions and apply the mass-delete guards *before touching anything*.
//! 5. Execute the remote → local side: local deletes → moves → folder creates → downloads → empty-folder cleanup.
//! 6. Two-way: execute the local → SharePoint side: new folders and files, in-place uploads, deletes.
//! 7. Save the new deltaLink only if nothing errored, so a failed run is simply replayed next time.
//!
//! Staging and planning run in one DB transaction (rolled back for a dry run); after that every change commits on its
//! own, so the DB always matches what was actually done on disk and in SharePoint.
//!
//! Every tracked file is re-evaluated against the disk each run (cheap stat; hash only on mismatch), so a run also
//! repairs local damage (deleted or corrupted files) and retries earlier errors.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result, bail};
use futures_util::{StreamExt, TryStreamExt};
use serde::{Deserialize, Serialize};
use looper_sharepoint_core::config::{Config, Library, LocalOrphans};
use looper_sharepoint_core::filter::Filter;
use looper_sharepoint_core::ignore::Ignore;
use looper_sharepoint_core::naming::{CharMap, NamingMode, local_key};
use looper_sharepoint_core::quickxor::{QuickXorHash, hash_file};
use looper_sharepoint_core::reconcile::{
    DeleteDecision, FileDecision, Local, RemoteFile, TwoWay, decide_file, decide_remote_delete, decide_remote_delete_two_way,
    decide_two_way,
};
use looper_sharepoint_core::state::{ItemRow, ItemState, StateDb, meta};
use looper_sharepoint_graph::models::DriveItem;
use looper_sharepoint_graph::{GraphClient, GraphError};
use tokio::io::AsyncWriteExt;

use crate::types::Progress;

mod two_way;

pub const TMP_DIR: &str = ".looper-sharepoint-tmp";
/// The mass-delete guard only applies when at least this many files would be deleted.
const MASS_DELETE_FLOOR: usize = 10;
const FOLDER_GONE: &str = "its folder is gone from SharePoint (deleted, or your access was removed)";
pub(crate) const READ_ONLY: &str = "SharePoint refused it: you can only read this folder or file";
pub(crate) const HIDDEN: &str =
    "it vanished from SharePoint but its name is still taken: your access to it was probably removed (or you can only read its folder)";
/// How often to list the whole library instead of asking for changes. Delta doesn't report items someone has been
/// given access to (or given back), so without this they'd only appear once they next change.
const FULL_SYNC_EVERY: chrono::Duration = chrono::Duration::hours(24);
const OS_JUNK: &[&str] = &[".DS_Store", "Thumbs.db", "desktop.ini"];

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunOptions {
    /// Plan only: report what would happen, change nothing.
    #[serde(default)]
    pub dry_run: bool,
    /// Override the mass-delete guard for this run.
    #[serde(default)]
    pub allow_mass_delete: bool,
}

/// The mass-delete guard refused the run before anything changed.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct MassDeleteRefused(pub String);

/// Receives progress events during a run. Must be cheap; it's called inline.
pub type ProgressFn<'a> = &'a (dyn Fn(Progress) + Send + std::marker::Sync);

/// A progress callback that ignores everything.
pub fn no_progress(_: Progress) {}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Report {
    pub library: String,
    pub local_root: String,
    pub full_enumeration: bool,
    pub resynced: bool,
    pub delta_items: usize,
    pub downloaded: usize,
    pub bytes_downloaded: u64,
    pub adopted: usize,
    pub moved: usize,
    pub folders_created: usize,
    pub deleted_local: Vec<String>,
    pub local_edits_kept: Vec<String>,
    pub conflicts: Vec<Issue>,
    pub skipped: Vec<Issue>,
    pub errors: Vec<Issue>,
    pub orphans: Vec<String>,
    pub orphans_deleted: usize,
    /// Two-way: files uploaded (new or in place).
    pub uploaded: usize,
    pub bytes_uploaded: u64,
    /// Two-way: folders created in SharePoint.
    pub created_remote: usize,
    /// Two-way: local renames/moves applied in SharePoint.
    pub moved_remote: usize,
    /// Two-way: deleted in SharePoint because they were deleted locally (recoverable from the site recycle bin).
    pub deleted_remote: Vec<String>,
    /// Two-way: local versions kept as conflict copies (and uploaded).
    pub conflict_copies: Vec<String>,
    /// Two-way: local files that are not uploaded because the library's filter excludes them.
    pub local_only: Vec<String>,
    pub delta_saved: bool,
    pub dry_run: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Issue {
    pub path: String,
    pub reason: String,
}

impl Report {
    pub fn has_problems(&self) -> bool {
        !self.errors.is_empty() || !self.conflicts.is_empty()
    }
}

pub struct Resolved {
    pub site_id: String,
    pub drive_id: String,
    /// The synced folder's item id, for a library limited to `folder`. It's followed by id, so a rename or move in
    /// SharePoint changes nothing locally.
    pub folder_id: Option<String>,
}

impl Resolved {
    /// The library's state DB. A folder has its own, so two folders of one SharePoint library never share state.
    pub fn state_db(&self, data_dir: &Path) -> PathBuf {
        match &self.folder_id {
            Some(f) => StateDb::path_for(data_dir, &format!("{}/{f}", self.drive_id)),
            None => StateDb::path_for(data_dir, &self.drive_id),
        }
    }
}

pub async fn resolve(graph: &GraphClient, lib: &Library) -> Result<Resolved> {
    let site = match graph.site_by_url(&lib.site).await {
        Ok(site) => site,
        Err(e) if e.is_forbidden() => anyhow::bail!(
            "no access to site {}: the signed-in account isn't a member (see `looper-sharepoint whoami`). \
             Ask a site owner to add it, or sign in as an account that is",
            lib.site
        ),
        Err(e) => return Err(anyhow::Error::from(e).context(format!("resolving site {}", lib.site))),
    };
    let drives = graph.site_drives(&site.id).await?;
    let drive = drives
        .iter()
        .find(|d| d.id == lib.library || d.name.as_deref().is_some_and(|n| n.eq_ignore_ascii_case(&lib.library)))
        .with_context(|| {
            let names: Vec<_> = drives.iter().filter_map(|d| d.name.clone()).collect();
            format!("library {:?} not found in {} (available: {})", lib.library, lib.site, names.join(", "))
        })?;
    let folder_id = match &lib.folder {
        None => None,
        Some(folder) => match graph.item_by_path(&drive.id, folder).await {
            Ok(it) if it.is_dir() => Some(it.id),
            Ok(_) => bail!("{folder:?} in {} is a file; `folder` must be a folder", lib.site),
            Err(GraphError::NotFound(_)) => {
                bail!("folder {folder:?} isn't in library {:?} at {} (or you can't see it)", lib.library, lib.site)
            }
            Err(e) => return Err(anyhow::Error::from(e).context(format!("finding folder {folder:?}"))),
        },
    };
    Ok(Resolved { site_id: site.id, drive_id: drive.id.clone(), folder_id })
}

/// Sync one configured library once.
///
/// `data_dir` holds the state DBs (see [`looper_sharepoint_core::config::data_dir`] for the default). The caller is
/// responsible for making sure only one process syncs a given data dir at a time; see [`crate::HostLock`].
pub async fn sync_library(
    graph: &GraphClient,
    cfg: &Config,
    data_dir: &Path,
    lib: &Library,
    opts: &RunOptions,
    progress: ProgressFn<'_>,
) -> Result<Report> {
    let resolved = match crate::index::cached(data_dir, lib) {
        Some(r) => r,
        None => resolve(graph, lib).await?,
    };
    let local_root = cfg.local_root(lib);
    let db = StateDb::open(&resolved.state_db(data_dir))?;
    crate::index::record(data_dir, lib, &resolved, db.path())?;

    let mut s = Sync {
        graph,
        cfg,
        lib,
        drive_id: resolved.drive_id.clone(),
        folder_id: resolved.folder_id.clone(),
        root: local_root.clone(),
        db,
        filter: Filter::new(&lib.include, &lib.exclude)?,
        ignore: Ignore::new(&cfg.sync.ignore)?,
        two_way: cfg.uploads(lib),
        uploads: vec![],
        remote_deletes: vec![],
        missing_dirs: vec![],
        reuploads: HashSet::new(),
        set_aside_stamp: None,
        map: cfg.char_map()?,
        mode: cfg.naming.mode,
        report: Report { library: lib.local.clone(), local_root: local_root.display().to_string(), dry_run: opts.dry_run, ..Default::default() },
        opts: opts.clone(),
        progress,
    };
    s.db.set_meta(meta::SITE_ID, &resolved.site_id)?;
    s.db.set_meta(meta::DRIVE_ID, &resolved.drive_id)?;
    s.db.set_meta(meta::LOCAL_ROOT, &local_root.display().to_string())?;
    // Two-way: a vanished local folder must not look like "everything was deleted" (check before recreating it).
    if s.two_way {
        s.check_local_root()?;
    }
    if !opts.dry_run {
        std::fs::create_dir_all(&local_root).with_context(|| format!("creating {}", local_root.display()))?;
    }

    s.db.begin()?;
    let result = s.run().await;
    // The cached drive id is stale if the library went away (deleted, or the config now points elsewhere).
    if let Err(e) = &result
        && e.chain().any(|c| matches!(c.downcast_ref::<GraphError>(), Some(GraphError::NotFound(_))))
    {
        let _ = crate::index::invalidate(data_dir, &lib.local);
    }
    // The run commits after planning; only staging/planning can still be open here (and a dry run always is).
    if s.db.in_transaction() {
        match (&result, opts.dry_run) {
            (Ok(()), false) => s.db.commit()?,
            _ => s.db.rollback()?,
        }
    }
    result?;
    Ok(s.report)
}

struct Sync<'a> {
    progress: ProgressFn<'a>,
    graph: &'a GraphClient,
    cfg: &'a Config,
    lib: &'a Library,
    drive_id: String,
    /// The synced folder, for a library limited to `folder`: the root of everything below.
    folder_id: Option<String>,
    root: PathBuf,
    db: StateDb,
    filter: Filter,
    ignore: Ignore,
    map: CharMap,
    mode: NamingMode,
    report: Report,
    opts: RunOptions,
    /// Upload local changes (two-way, not read-only).
    two_way: bool,
    /// Two-way work found while applying remote changes, executed in the local → SharePoint phase:
    /// in-place uploads `(item_id, path)` and remote deletes `(item_id, path)`.
    uploads: Vec<(String, String)>,
    remote_deletes: Vec<(String, String)>,
    /// Two-way: tracked folders whose local directory is gone (candidates for deleting in SharePoint).
    missing_dirs: Vec<String>,
    /// Files deleted in SharePoint but edited locally, to upload again (paths), once their name proves free.
    reuploads: HashSet<String>,
    /// The time-stamped folder this run sets files aside in (see [`Sync::set_aside`]), once one is needed.
    set_aside_stamp: Option<String>,
}

/// Where an item should live locally.
#[derive(Debug, Clone)]
enum Target {
    /// Relative local path (`/`-separated).
    Path(String),
    /// Not synced; reason.
    Skip(String),
}

impl Sync<'_> {
    async fn run(&mut self) -> Result<()> {
        let (items, new_link) = self.pull_delta().await?;
        self.report.delta_items = items.len();
        let deleted_ids = self.stage(&items)?;

        let mut rows: HashMap<String, ItemRow> = self.db.all()?.into_iter().map(|r| (r.item_id.clone(), r)).collect();
        let deleted: HashSet<String> = self.expand_deletes(&deleted_ids, &rows)?;
        if self.two_way {
            // Local renames/moves are applied in SharePoint first (they don't delete anything), so the rest of the run
            // sees them as plain renames instead of a delete plus an upload.
            if !self.opts.dry_run && self.apply_local_moves(&rows, &deleted).await? {
                rows = self.db.all()?.into_iter().map(|r| (r.item_id.clone(), r)).collect();
            }
        }
        let targets = self.targets(&rows, &deleted);

        // Plan deletes in both directions, then the guards, before any change on disk or in SharePoint.
        let delete_plan = self.plan_deletes(&rows, &deleted)?;
        let n_delete = delete_plan.iter().filter(|(_, d)| *d == DeleteDecision::DeleteLocal).count();
        self.guard("remote deletions", n_delete)?;
        if self.two_way {
            // A dry run hasn't applied local moves, so don't count them as deletes.
            let moved = if self.opts.dry_run {
                self.detect_local_moves(&rows, &deleted)?.map(|m| m.moved_ids()).unwrap_or_default()
            } else {
                HashSet::new()
            };
            let n = self.count_local_deletes(&rows, &deleted, &targets, &moved);
            self.guard_remote("local deletions", n)?;
        }
        (self.progress)(Progress::Planned { changes: self.report.delta_items, deletes: n_delete });

        if self.opts.dry_run {
            self.describe_dry_run(&rows, &targets, &delete_plan)?;
            if self.two_way {
                self.describe_local_changes(&rows, &deleted)?;
            }
            return Ok(());
        }
        self.db.commit()?;

        if let Some(j) = self.db.get_meta(meta::REUPLOADS)? {
            self.reuploads = serde_json::from_str(&j).unwrap_or_default();
        }
        self.apply_deletes(&rows, delete_plan)?;
        self.apply_moves(&targets)?;
        self.apply_files_and_folders(&targets).await?;
        self.cleanup_deleted_dirs(&rows, &deleted)?;
        if self.two_way {
            self.push_local().await?;
        } else {
            self.handle_orphans()?;
        }

        if self.report.errors.is_empty() {
            self.db.set_meta(meta::DELTA_LINK, &new_link)?;
            let now = chrono::Utc::now().to_rfc3339();
            self.db.set_meta(meta::LAST_SYNC_AT, &now)?;
            if self.report.full_enumeration {
                self.db.set_meta(meta::LAST_FULL_SYNC_AT, &now)?;
            }
            self.report.delta_saved = true;
        }
        Ok(())
    }

    /// What changed in SharePoint since the last run (everything, on a full listing), de-duplicated by id, plus the
    /// new deltaLink. For a folder, only what's inside it: see [`Sync::in_folder`].
    async fn pull_delta(&mut self) -> Result<(Vec<DriveItem>, String)> {
        let mut link = self.db.get_meta(meta::DELTA_LINK)?;
        if link.is_some() && full_sync_due(self.db.get_meta(meta::LAST_FULL_SYNC_AT)?.as_deref(), chrono::Utc::now()) {
            tracing::info!("daily full listing, to pick up items shared with you since");
            link = None;
        }
        loop {
            self.report.full_enumeration = link.is_none();
            let result = match (self.folder_id.clone(), link.as_deref()) {
                (Some(f), None) => self.list_folder(&f).await,
                (Some(f), Some(l)) => match self.delta_all(Some(l)).await {
                    Ok((items, new_link)) => self.in_folder(&f, items).await.map(|items| (items, new_link)),
                    Err(e) => Err(e),
                },
                (None, l) => self.delta_all(l).await,
            };
            match result {
                Err(e) if matches!(e.downcast_ref::<GraphError>(), Some(GraphError::ResyncRequired)) && link.is_some() => {
                    tracing::warn!("delta token expired; re-enumerating library");
                    self.report.resynced = true;
                    link = None;
                }
                r => return r,
            }
        }
    }

    /// All delta pages from `link` (or a full enumeration of the library), de-duplicated by id (the last occurrence
    /// wins), plus the new deltaLink.
    async fn delta_all(&mut self, link: Option<&str>) -> Result<(Vec<DriveItem>, String)> {
        let mut by_id: HashMap<String, usize> = HashMap::new();
        let mut items: Vec<Option<DriveItem>> = vec![];
        let mut next = link.map(str::to_string);
        let mut first = true;
        loop {
            let page = self.graph.delta_page(&self.drive_id, if first { link } else { next.as_deref() }).await?;
            first = false;
            for it in page.value {
                if let Some(prev) = by_id.insert(it.id.clone(), items.len()) {
                    items[prev] = None;
                }
                items.push(Some(it));
            }
            if let Some(d) = page.delta_link {
                return Ok((items.into_iter().flatten().collect(), d));
            }
            next = Some(page.next_link.context("delta page with neither nextLink nor deltaLink")?);
        }
    }

    /// A folder's full listing: the folder and everything in it, plus a deltaLink taken *before* listing, so a change
    /// made while listing is seen again next run rather than missed. (Delta itself only runs from the library's top.)
    async fn list_folder(&mut self, folder_id: &str) -> Result<(Vec<DriveItem>, String)> {
        let link = self.graph.delta_latest(&self.drive_id).await?;
        let folder = match self.graph.item(&self.drive_id, folder_id).await {
            Ok(f) if f.is_dir() && !f.is_deleted() => f,
            Ok(_) => bail!(self.folder_gone()),
            Err(e) if matches!(e, GraphError::NotFound(_)) || e.is_forbidden() => bail!(self.folder_gone()),
            Err(e) => return Err(e.into()),
        };
        let mut out = vec![folder];
        out.extend(self.list_subtree(folder_id).await?);
        Ok((out, link))
    }

    /// Everything below folder `id` (not the folder itself).
    async fn list_subtree(&mut self, id: &str) -> Result<Vec<DriveItem>> {
        let mut out = vec![];
        let mut queue = vec![id.to_string()];
        while let Some(id) = queue.pop() {
            for c in self.graph.children(&self.drive_id, &id).await? {
                if c.is_dir() {
                    queue.push(c.id.clone());
                }
                out.push(c);
            }
        }
        Ok(out)
    }

    /// Keep the changes inside the synced folder. Delta lists the whole library, so:
    /// - an item is inside if its parent chain reaches the folder, through this batch or what's already tracked;
    /// - a tracked item that's now outside was moved out: it's passed on as deleted;
    /// - a folder that's new here (moved in from elsewhere, whose contents delta doesn't list again) is listed.
    async fn in_folder(&mut self, folder_id: &str, items: Vec<DriveItem>) -> Result<Vec<DriveItem>> {
        if items.iter().any(|it| it.id == folder_id && it.is_deleted()) {
            bail!(self.folder_gone());
        }
        let batch: HashMap<String, DriveItem> = items.iter().map(|it| (it.id.clone(), it.clone())).collect();
        let mut memo: HashMap<String, bool> = HashMap::new();
        let mut out = vec![];
        let mut new_dirs = vec![];
        for it in items {
            let known = self.db.get(&it.id)?.is_some();
            if self.inside(folder_id, &it.id, &batch, &mut memo, 0)? {
                if it.is_dir() && !it.is_deleted() && !known && it.id != folder_id {
                    new_dirs.push(it.id.clone());
                }
                out.push(it);
            } else if known {
                let mut gone = it;
                gone.deleted = Some(serde_json::json!({}));
                out.push(gone);
            }
        }
        let mut seen: HashSet<String> = out.iter().map(|it| it.id.clone()).collect();
        for d in new_dirs {
            for it in self.list_subtree(&d).await? {
                if seen.insert(it.id.clone()) {
                    out.push(it);
                }
            }
        }
        Ok(out)
    }

    fn inside(&self, folder_id: &str, id: &str, batch: &HashMap<String, DriveItem>, memo: &mut HashMap<String, bool>, depth: usize) -> Result<bool> {
        if id == folder_id {
            return Ok(true);
        }
        if let Some(&m) = memo.get(id) {
            return Ok(m);
        }
        let r = match batch.get(id) {
            // A deletion carries no reliable parent: it matters only if it's tracked.
            Some(it) if it.is_deleted() => self.db.get(id)?.is_some(),
            Some(it) => match it.parent_id() {
                Some(p) if depth < 400 && !it.is_root() => self.inside(folder_id, p, batch, memo, depth + 1)?,
                _ => false,
            },
            // Unchanged: inside if it's tracked (only items inside are).
            None => self.db.get(id)?.is_some(),
        };
        memo.insert(id.to_string(), r);
        Ok(r)
    }

    fn folder_gone(&self) -> String {
        format!(
            "the SharePoint folder {:?} is gone, or no longer shared with you. Nothing was deleted locally. If it was \
             deleted, it's in the site's recycle bin; if it moved somewhere you can't see, or you want a different \
             folder, run `looper-sharepoint remove {}` and add it again",
            self.lib.folder.as_deref().unwrap_or_default(),
            self.lib.name()
        )
    }

    /// Write remote state into the DB. Returns ids reported deleted (plus, on a full enumeration, ids not seen).
    fn stage(&mut self, items: &[DriveItem]) -> Result<Vec<String>> {
        let mut deleted = vec![];
        let mut seen = HashSet::new();
        for it in items {
            seen.insert(it.id.clone());
            if it.is_deleted() {
                deleted.push(it.id.clone());
                continue;
            }
            let mut row = match self.db.get(&it.id)? {
                Some(r) => r,
                None => ItemRow::new(&it.id, "", it.is_dir()),
            };
            row.is_root = match &self.folder_id {
                Some(f) => it.id == *f,
                None => it.is_root(),
            };
            row.is_dir = it.is_dir();
            row.parent_id = if row.is_root { None } else { it.parent_id().map(str::to_string) };
            row.remote_name = it.name.clone().unwrap_or_default();
            row.r_etag = it.e_tag.clone();
            row.r_quickxor = it.quickxor().map(str::to_string);
            row.r_size = it.size;
            row.r_mtime = it.mtime().map(str::to_string);
            if row.is_root {
                row.local_path = Some(String::new());
                row.local_key = Some(String::new());
                row.state = ItemState::Synced;
            }
            self.db.put(&row)?;
        }
        if self.report.full_enumeration {
            for r in self.db.all()? {
                if !seen.contains(&r.item_id) {
                    deleted.push(r.item_id);
                }
            }
        }
        Ok(deleted)
    }

    /// A deleted folder deletes its whole subtree, even if delta didn't list the children.
    fn expand_deletes(&self, ids: &[String], rows: &HashMap<String, ItemRow>) -> Result<HashSet<String>> {
        let mut out = HashSet::new();
        for id in ids {
            if rows.contains_key(id) {
                out.extend(self.db.subtree(id)?.into_iter().map(|r| r.item_id));
            }
        }
        Ok(out)
    }

    /// Desired local path for every live item. Parents are resolved before children (memoized).
    fn targets(&self, rows: &HashMap<String, ItemRow>, deleted: &HashSet<String>) -> HashMap<String, Target> {
        fn resolve(
            id: &str,
            rows: &HashMap<String, ItemRow>,
            map: &CharMap,
            mode: NamingMode,
            memo: &mut HashMap<String, (String, Target)>,
            depth: usize,
        ) -> (String, Target) {
            if let Some(t) = memo.get(id) {
                return t.clone();
            }
            let Some(row) = rows.get(id) else {
                return (String::new(), Target::Skip("parent folder unknown".into()));
            };
            let out = if row.is_root {
                (String::new(), Target::Path(String::new()))
            } else if depth > 400 {
                (String::new(), Target::Skip("folder cycle".into()))
            } else {
                let parent = row.parent_id.clone().unwrap_or_default();
                let (premote, ptarget) = resolve(&parent, rows, map, mode, memo, depth + 1);
                let remote = if premote.is_empty() { row.remote_name.clone() } else { format!("{premote}/{}", row.remote_name) };
                let target = match ptarget {
                    Target::Skip(r) => Target::Skip(r),
                    Target::Path(p) => match map.to_local(&row.remote_name, mode) {
                        Ok(name) => Target::Path(if p.is_empty() { name } else { format!("{p}/{name}") }),
                        Err(reason) => Target::Skip(reason),
                    },
                };
                (remote, target)
            };
            memo.insert(id.to_string(), out.clone());
            out
        }

        let mut memo = HashMap::new();
        let mut live: Vec<&ItemRow> = rows.values().filter(|r| !deleted.contains(&r.item_id) && !r.is_root).collect();
        // Already-materialized items keep their names in a collision; then a stable order.
        live.sort_by(|a, b| b.is_materialized().cmp(&a.is_materialized()).then(a.item_id.cmp(&b.item_id)));

        let mut out = HashMap::new();
        let mut needed_dirs: HashSet<String> = HashSet::new();
        let mut claimed: HashMap<String, String> = HashMap::new();
        // An excluded folder prunes its whole subtree.
        let mut excluded_dirs: Vec<String> = vec![];
        for row in live.iter().filter(|r| r.is_dir) {
            let (remote, _) = resolve(&row.item_id, rows, &self.map, self.mode, &mut memo, 0);
            if self.filter.excludes_dir(&remote) {
                excluded_dirs.push(format!("{remote}/"));
            }
        }
        for row in live {
            let (remote, target) = resolve(&row.item_id, rows, &self.map, self.mode, &mut memo, 0);
            let target = match target {
                Target::Skip(r) => Target::Skip(r),
                _ if excluded_dirs.iter().any(|d| remote.starts_with(d.as_str())) => Target::Skip("excluded by filter".into()),
                Target::Path(p) if row.is_dir => {
                    if self.filter.includes_dir(&remote) {
                        needed_dirs.insert(p.clone());
                    }
                    Target::Path(p)
                }
                Target::Path(p) => {
                    if self.filter.includes_file(&remote) {
                        Target::Path(p)
                    } else {
                        Target::Skip("excluded by filter".into())
                    }
                }
            };
            let target = match target {
                Target::Path(p) => match claimed.get(&local_key(&p)) {
                    Some(other) if other != &row.item_id => Target::Skip(format!("local name collides with another item at {p:?}")),
                    _ => {
                        claimed.insert(local_key(&p), row.item_id.clone());
                        if !row.is_dir {
                            // Every ancestor folder of an included file is needed.
                            let mut a = p.as_str();
                            while let Some((parent, _)) = a.rsplit_once('/') {
                                needed_dirs.insert(parent.to_string());
                                a = parent;
                            }
                        }
                        Target::Path(p)
                    }
                },
                skip => skip,
            };
            out.insert(row.item_id.clone(), target);
        }
        // Folders that aren't needed (filtered and holding no included files) are skipped.
        for row in rows.values().filter(|r| r.is_dir && !r.is_root) {
            if let Some(Target::Path(p)) = out.get(&row.item_id)
                && !needed_dirs.contains(p) {
                    out.insert(row.item_id.clone(), Target::Skip("excluded by filter".into()));
                }
        }
        out
    }

    fn abs(&self, rel: &str) -> PathBuf {
        let mut p = self.root.clone();
        for seg in rel.split('/').filter(|s| !s.is_empty()) {
            p.push(seg);
        }
        p
    }

    /// Compare the file at `rel` with the row's base.
    fn local_state(&self, rel: &str, row: &ItemRow) -> Result<Local> {
        let abs = self.abs(rel);
        let md = match std::fs::symlink_metadata(&abs) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Local::Missing),
            Err(e) => return Err(e).with_context(|| format!("stat {}", abs.display())),
        };
        if !md.is_file() {
            return Ok(Local::Differs { quickxor: String::new() });
        }
        let at_base = row.local_path.as_deref() == Some(rel);
        if at_base && row.l_size == Some(md.len() as i64) && row.l_mtime_ns == Some(mtime_ns(&md)) {
            return Ok(Local::Same);
        }
        let h = hash_file(&abs).with_context(|| format!("hashing {}", abs.display()))?;
        if at_base && row.l_quickxor.as_deref() == Some(h.as_str()) {
            return Ok(Local::Same);
        }
        Ok(Local::Differs { quickxor: h })
    }

    fn guard(&self, what: &str, n: usize) -> Result<()> {
        let tracked = self.db.materialized_file_count()? as f64;
        if n >= MASS_DELETE_FLOOR && (n as f64) > tracked * self.cfg.sync.max_delete_fraction && !self.opts.allow_mass_delete {
            return Err(MassDeleteRefused(format!(
                "{what}: this run would delete {n} of {tracked} local files in {:?}, more than sync.max_delete_fraction ({}). \
                 Nothing was changed. Re-run with --allow-mass-delete if this is expected.",
                self.lib.local,
                self.cfg.sync.max_delete_fraction
            ))
            .into());
        }
        Ok(())
    }

    /// Refuse a run that would delete more than `max_delete_fraction` of the library *in SharePoint*.
    fn guard_remote(&self, what: &str, n: usize) -> Result<()> {
        let tracked = self.db.materialized_file_count()? as f64;
        if n >= MASS_DELETE_FLOOR && (n as f64) > tracked * self.cfg.sync.max_delete_fraction && !self.opts.allow_mass_delete {
            return Err(MassDeleteRefused(format!(
                "{what}: this run would delete {n} of {tracked} files in SharePoint (library {:?}), more than \
                 sync.max_delete_fraction ({}). Nothing was changed. If you meant to delete them, re-run with \
                 --allow-mass-delete (or \"Allow these deletions\" in the tray); they go to the site's recycle bin.",
                self.lib.local,
                self.cfg.sync.max_delete_fraction
            ))
            .into());
        }
        Ok(())
    }

    fn plan_deletes(&self, rows: &HashMap<String, ItemRow>, deleted: &HashSet<String>) -> Result<Vec<(String, DeleteDecision)>> {
        let mut plan = vec![];
        for id in deleted {
            let row = &rows[id];
            if row.is_dir {
                continue;
            }
            let local = match &row.local_path {
                Some(p) => self.local_state(p, row)?,
                None => Local::Missing,
            };
            // Its folder went too: deleted with it, or this user lost access to the folder (it looks the same).
            let folder_gone = row.parent_id.as_ref().is_some_and(|p| deleted.contains(p));
            plan.push((
                id.clone(),
                if self.two_way {
                    decide_remote_delete_two_way(row, &local, self.cfg.sync.on_remote_delete, folder_gone)
                } else {
                    decide_remote_delete(row, &local, self.cfg.sync.on_remote_delete)
                },
            ));
        }
        Ok(plan)
    }

    fn apply_deletes(&mut self, rows: &HashMap<String, ItemRow>, plan: Vec<(String, DeleteDecision)>) -> Result<()> {
        for (id, decision) in plan {
            let row = &rows[&id];
            let path = row.local_path.clone().unwrap_or_default();
            match decision {
                DeleteDecision::DeleteLocal => {
                    let abs = self.abs(&path);
                    match std::fs::remove_file(&abs) {
                        Ok(()) => {}
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                        Err(e) => {
                            self.issue_error(&path, format!("deleting: {e}"));
                            continue;
                        }
                    }
                    tracing::info!(path, item = id, "deleted locally (removed in SharePoint; recoverable from the site recycle bin)");
                    (self.progress)(Progress::Deleted { path: path.clone() });
                    self.report.deleted_local.push(path);
                    self.db.remove(&id)?;
                }
                DeleteDecision::Forget | DeleteDecision::ForgetKeepFile => self.db.remove(&id)?,
                DeleteDecision::KeepModified => {
                    self.report.conflicts.push(Issue { path, reason: "deleted in SharePoint but modified locally; kept".into() });
                    self.db.remove(&id)?;
                }
                DeleteDecision::Reupload => {
                    // The file becomes untracked and is uploaded as a new item by `push_local`.
                    tracing::info!(path, "deleted in SharePoint but edited locally; uploading it again");
                    self.reuploads.insert(path.clone());
                    self.save_reuploads()?;
                    self.report.local_edits_kept.push(path);
                    self.db.remove(&id)?;
                }
                DeleteDecision::SetAside => {
                    if self.set_aside(&path, FOLDER_GONE)? {
                        self.db.remove(&id)?;
                    }
                }
            }
        }
        // Folder rows are removed in cleanup_deleted_dirs, after their now-empty directories are gone.
        Ok(())
    }

    /// Renames/moves of already-materialized items, shallowest first, so a folder move carries its children.
    fn apply_moves(&mut self, targets: &HashMap<String, Target>) -> Result<()> {
        let mut moves: Vec<(String, String, String)> = vec![]; // (id, from, to)
        for (id, t) in targets {
            if let (Target::Path(to), Some(row)) = (t, self.db.get(id)?)
                && let Some(from) = row.local_path.as_ref().filter(|f| *f != to) {
                    moves.push((id.clone(), from.clone(), to.clone()));
                }
        }
        moves.sort_by_key(|(_, _, to)| to.matches('/').count());
        for (id, _, to) in moves {
            // Re-read: an earlier folder move may already have relocated this item.
            let row = self.db.get(&id)?.context("row vanished")?;
            let from = row.local_path.clone().unwrap_or_default();
            if from == to {
                continue;
            }
            let (src, dst) = (self.abs(&from), self.abs(&to));
            let already_moved = !src.exists() && dst.exists();
            if dst.exists() && local_key(&from) != local_key(&to) && !already_moved {
                self.issue_conflict(&to, "moved in SharePoint, but something already exists at the new local path".into());
                continue;
            }
            if src.exists() && !already_moved {
                if let Some(parent) = dst.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                if let Err(e) = std::fs::rename(&src, &dst) {
                    self.issue_error(&to, format!("moving from {from}: {e}"));
                    continue;
                }
            }
            self.report.moved += 1;
            (self.progress)(Progress::Moved { from: from.clone(), to: to.clone() });
            // Rewrite this item's and all descendants' local paths.
            for mut r in self.db.subtree(&id)? {
                if let Some(p) = r.local_path.as_deref()
                    && (p == from || p.starts_with(&format!("{from}/"))) {
                        let np = format!("{to}{}", &p[from.len()..]);
                        r.local_key = Some(local_key(&np));
                        r.local_path = Some(np);
                        self.db.put(&r)?;
                    }
            }
        }
        Ok(())
    }

    async fn apply_files_and_folders(&mut self, targets: &HashMap<String, Target>) -> Result<()> {
        // Folders first (shallowest first), then files.
        let mut dirs: Vec<(&String, &String)> = vec![];
        let mut downloads: Vec<(ItemRow, String)> = vec![];
        let mut ordered: Vec<(&String, &Target)> = targets.iter().collect();
        ordered.sort_by_key(|(_, t)| match t {
            Target::Path(p) => p.matches('/').count(),
            Target::Skip(_) => 0,
        });

        for (id, t) in &ordered {
            let mut row = self.db.get(id)?.context("row vanished")?;
            match t {
                Target::Skip(reason) => {
                    if row.state != ItemState::Skipped || row.reason.as_deref() != Some(reason) {
                        if row.is_materialized() {
                            tracing::info!(path = row.local_path.as_deref(), reason, "no longer synced; local copy left in place");
                        }
                        row.local_path = None;
                        row.local_key = None;
                        row.l_size = None;
                        row.l_mtime_ns = None;
                        row.l_quickxor = None;
                        row.state = ItemState::Skipped;
                        row.reason = Some(reason.clone());
                        self.db.put(&row)?;
                    }
                    if reason != "excluded by filter" {
                        self.report.skipped.push(Issue { path: row.remote_name.clone(), reason: reason.clone() });
                    }
                }
                Target::Path(p) if row.is_dir => dirs.push((id, p)),
                Target::Path(p) if self.two_way => {
                    let local = self.local_state(p, &row)?;
                    let remote = RemoteFile { quickxor: row.r_quickxor.clone(), size: row.r_size.unwrap_or(0) };
                    let base = row.is_materialized().then_some(&row);
                    match decide_two_way(base, &remote, &local) {
                        TwoWay::Download => downloads.push((row, p.clone())),
                        TwoWay::Restore => {
                            tracing::info!(path = p, "deleted locally but changed in SharePoint; restoring");
                            downloads.push((row, p.clone()));
                        }
                        TwoWay::Adopt => {
                            self.record_local(&mut row, p)?;
                            self.report.adopted += 1;
                        }
                        TwoWay::Nothing => {
                            if row.state != ItemState::Synced {
                                row.state = ItemState::Synced;
                                row.reason = None;
                                self.db.put(&row)?;
                            }
                        }
                        TwoWay::Upload => self.uploads.push((row.item_id.clone(), p.clone())),
                        TwoWay::DeleteRemote => self.remote_deletes.push((row.item_id.clone(), p.clone())),
                        TwoWay::ConflictCopy => {
                            if self.keep_conflict_copy(p)? {
                                downloads.push((row, p.clone()));
                            }
                        }
                    }
                }
                Target::Path(p) => {
                    let local = self.local_state(p, &row)?;
                    let remote = RemoteFile { quickxor: row.r_quickxor.clone(), size: row.r_size.unwrap_or(0) };
                    let base = row.is_materialized().then_some(&row);
                    match decide_file(base, &remote, &local) {
                        FileDecision::Download => downloads.push((row, p.clone())),
                        FileDecision::Adopt => {
                            self.record_local(&mut row, p)?;
                            self.report.adopted += 1;
                        }
                        FileDecision::Nothing => {
                            if row.state != ItemState::Synced {
                                row.state = ItemState::Synced;
                                row.reason = None;
                                self.db.put(&row)?;
                            }
                        }
                        FileDecision::KeepLocalEdit => self.report.local_edits_kept.push(p.clone()),
                        FileDecision::Conflict(reason) => {
                            row.state = ItemState::Conflict;
                            row.reason = Some(reason.into());
                            self.db.put(&row)?;
                            self.issue_conflict(p, reason.into());
                        }
                    }
                }
            }
        }

        for (id, p) in dirs {
            let abs = self.abs(p);
            if self.two_way && !abs.exists() && self.db.get(id)?.is_some_and(|r| r.local_path.as_deref() == Some(p.as_str())) {
                // A folder we synced before is gone locally: deleted (or moved, already handled) here. Don't recreate
                // it; the local → SharePoint phase decides whether to delete it there.
                self.missing_dirs.push(id.clone());
                continue;
            }
            if !abs.is_dir() {
                if abs.exists() {
                    self.issue_conflict(p, "a file exists where SharePoint has a folder".into());
                    continue;
                }
                std::fs::create_dir_all(&abs).with_context(|| format!("creating {}", abs.display()))?;
                self.report.folders_created += 1;
            }
            let mut row = self.db.get(id)?.context("row vanished")?;
            row.local_path = Some(p.clone());
            row.local_key = Some(local_key(p));
            row.state = ItemState::Synced;
            row.reason = None;
            if let Err(e) = self.db.put(&row) {
                self.issue_error(p, format!("recording folder: {e}"));
            }
        }

        self.download_all(downloads).await
    }

    async fn download_all(&mut self, jobs: Vec<(ItemRow, String)>) -> Result<()> {
        let tmp = self.root.join(TMP_DIR);
        if !jobs.is_empty() {
            std::fs::create_dir_all(&tmp)?;
            hide(&tmp);
        }
        let graph = self.graph.clone();
        let drive = Arc::new(self.drive_id.clone());
        let root = self.root.clone();
        let total = jobs.len();
        let total_bytes: u64 = jobs.iter().map(|(r, _)| r.r_size.unwrap_or(0).max(0) as u64).sum();
        if total > 0 {
            (self.progress)(Progress::Downloading { done: 0, total, bytes_done: 0, bytes_total: total_bytes });
        }
        let mut results = futures_util::stream::iter(jobs)
            .map(|(row, rel)| {
                let (graph, drive, tmp, root) = (graph.clone(), drive.clone(), tmp.clone(), root.clone());
                async move {
                    let r = download_one(&graph, &drive, &row, &rel, &tmp, &root).await;
                    (row, rel, r)
                }
            })
            .buffer_unordered(self.cfg.sync.max_parallel_downloads);
        // Record each file as it lands, so progress (and a later abandoned run) reflects reality.
        let mut done = 0;
        while let Some((mut row, rel, r)) = results.next().await {
            done += 1;
            match r {
                Ok(bytes) => {
                    self.report.downloaded += 1;
                    self.report.bytes_downloaded += bytes;
                    self.record_local(&mut row, &rel)?;
                    (self.progress)(Progress::Downloaded { path: rel });
                }
                Err(e) => {
                    row.state = ItemState::Error;
                    row.reason = Some(format!("{e:#}"));
                    self.db.put(&row)?;
                    self.issue_error(&rel, format!("{e:#}"));
                }
            }
            (self.progress)(Progress::Downloading {
                done,
                total,
                bytes_done: self.report.bytes_downloaded,
                bytes_total: total_bytes,
            });
        }
        let _ = std::fs::remove_dir(&tmp);
        Ok(())
    }

    /// Record the file at `rel` (which has the remote content) as the new base.
    fn record_local(&mut self, row: &mut ItemRow, rel: &str) -> Result<()> {
        let md = std::fs::metadata(self.abs(rel))?;
        row.local_path = Some(rel.to_string());
        row.local_key = Some(local_key(rel));
        row.l_size = Some(md.len() as i64);
        row.l_mtime_ns = Some(mtime_ns(&md));
        row.l_quickxor = row.r_quickxor.clone();
        row.state = ItemState::Synced;
        row.reason = None;
        self.db.put(row)?;
        Ok(())
    }

    /// Remove directories of deleted folders once empty (deepest first); drop their rows.
    fn cleanup_deleted_dirs(&mut self, rows: &HashMap<String, ItemRow>, deleted: &HashSet<String>) -> Result<()> {
        let mut dirs: Vec<&ItemRow> = deleted.iter().map(|id| &rows[id]).filter(|r| r.is_dir).collect();
        dirs.sort_by_key(|r| std::cmp::Reverse(r.local_path.as_deref().map(|p| p.matches('/').count()).unwrap_or(0)));
        for r in dirs {
            if let Some(p) = &r.local_path {
                let abs = self.abs(p);
                // Two-way: whatever is still in it (new files, edits) would recreate the folder in SharePoint on
                // upload. If the folder went because this user lost access, that would leak restricted content.
                if self.two_way {
                    let mut left = vec![];
                    walk(&self.root, p, &mut |rel, is_dir| {
                        if !is_dir && !OS_JUNK.iter().any(|j| rel.ends_with(j)) {
                            left.push(rel.to_string());
                        }
                        true
                    })?;
                    for f in left {
                        self.set_aside(&f, FOLDER_GONE)?;
                    }
                    remove_empty_dirs(&abs);
                }
                for junk in OS_JUNK {
                    let _ = std::fs::remove_file(abs.join(junk));
                }
                if std::fs::remove_dir(&abs).is_ok() {
                    self.report.deleted_local.push(format!("{p}/"));
                }
            }
            self.db.remove(&r.item_id)?;
        }
        Ok(())
    }

    /// Move a local file out of the library folder to `<root>/not-uploaded/<library>/<time>/<path>`, so it's kept
    /// but never uploaded, and report it. Returns false (and reports an error) if it couldn't be moved.
    /// Move a local file SharePoint can't take to `not-uploaded/`, reporting `why` as a conflict.
    pub(crate) fn set_aside(&mut self, rel: &str, why: &str) -> Result<bool> {
        let stamp = self.set_aside_stamp.get_or_insert_with(|| chrono::Local::now().format("%Y-%m-%d %H%M%S").to_string()).clone();
        let mut dest = self.cfg.set_aside_dir(self.lib).join(stamp);
        for seg in rel.split('/') {
            dest.push(seg);
        }
        let src = self.abs(rel);
        let moved = if self.opts.dry_run {
            Ok(())
        } else {
            std::fs::create_dir_all(dest.parent().unwrap()).and_then(|_| std::fs::rename(&src, &dest))
        };
        match moved {
            Ok(()) => {
                tracing::warn!(path = rel, to = %dest.display(), why, "local changes moved out, not uploaded");
                self.report.conflicts.push(Issue {
                    path: rel.to_string(),
                    reason: format!("{why}, so your local copy wasn't uploaded. Moved to {}", dest.display()),
                });
                Ok(true)
            }
            Err(e) => {
                self.issue_error(rel, format!("{why}; moving it out of the library failed: {e}"));
                Ok(false)
            }
        }
    }

    /// Local files no row knows about.
    fn handle_orphans(&mut self) -> Result<()> {
        let known: HashSet<String> = self.db.all()?.into_iter().filter_map(|r| r.local_key).collect();
        let mut orphans = vec![];
        walk(&self.root, "", &mut |rel, is_dir| {
            if rel == TMP_DIR || rel.starts_with(&format!("{TMP_DIR}/")) {
                return false;
            }
            if !is_dir && !known.contains(&local_key(rel)) && !OS_JUNK.iter().any(|j| rel.ends_with(j)) {
                orphans.push(rel.to_string());
            }
            true
        })?;
        orphans.sort();
        if self.cfg.sync.local_orphans == LocalOrphans::Delete && !orphans.is_empty() {
            self.guard("local orphan deletion", orphans.len())?;
            for o in &orphans {
                match std::fs::remove_file(self.abs(o)) {
                    Ok(()) => self.report.orphans_deleted += 1,
                    Err(e) => self.issue_error(o, format!("deleting orphan: {e}")),
                }
            }
        }
        self.report.orphans = orphans;
        Ok(())
    }

    fn describe_dry_run(&mut self, rows: &HashMap<String, ItemRow>, targets: &HashMap<String, Target>, deletes: &[(String, DeleteDecision)]) -> Result<()> {
        for (id, d) in deletes {
            let path = rows[id].local_path.clone().unwrap_or_default();
            match d {
                DeleteDecision::DeleteLocal => self.report.deleted_local.push(path),
                DeleteDecision::SetAside => {
                    self.set_aside(&path, FOLDER_GONE)?;
                }
                _ => {}
            }
        }
        for (id, t) in targets {
            let row = &rows[id];
            match t {
                Target::Skip(r) if r != "excluded by filter" => self.report.skipped.push(Issue { path: row.remote_name.clone(), reason: r.clone() }),
                Target::Path(p) if !row.is_dir => {
                    let cur = row.local_path.as_deref().unwrap_or(p);
                    let local = self.local_state(cur, row)?;
                    let remote = RemoteFile { quickxor: row.r_quickxor.clone(), size: row.r_size.unwrap_or(0) };
                    if self.two_way {
                        match decide_two_way(row.is_materialized().then_some(row), &remote, &local) {
                            TwoWay::Download | TwoWay::Restore => {
                                self.report.downloaded += 1;
                                self.report.bytes_downloaded += row.r_size.unwrap_or(0) as u64;
                            }
                            TwoWay::Adopt => self.report.adopted += 1,
                            TwoWay::Upload => self.report.uploaded += 1,
                            TwoWay::DeleteRemote => self.report.deleted_remote.push(p.clone()),
                            TwoWay::ConflictCopy => self.report.conflicts.push(Issue { path: p.clone(), reason: "changed on both sides; a conflict copy would be kept".into() }),
                            TwoWay::Nothing => {}
                        }
                        continue;
                    }
                    match decide_file(row.is_materialized().then_some(row), &remote, &local) {
                        FileDecision::Download => {
                            self.report.downloaded += 1;
                            self.report.bytes_downloaded += row.r_size.unwrap_or(0) as u64;
                        }
                        FileDecision::Adopt => self.report.adopted += 1,
                        FileDecision::Conflict(r) => self.report.conflicts.push(Issue { path: p.clone(), reason: r.into() }),
                        FileDecision::KeepLocalEdit => self.report.local_edits_kept.push(p.clone()),
                        FileDecision::Nothing => {}
                    }
                    if row.local_path.as_deref().is_some_and(|c| c != p) {
                        self.report.moved += 1;
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    pub(crate) fn save_reuploads(&self) -> Result<()> {
        if self.reuploads.is_empty() {
            self.db.delete_meta(meta::REUPLOADS)?;
        } else {
            self.db.set_meta(meta::REUPLOADS, &serde_json::to_string(&self.reuploads)?)?;
        }
        Ok(())
    }

    fn issue_error(&mut self, path: &str, reason: String) {
        tracing::error!(path, reason, "sync error");
        (self.progress)(Progress::Error { path: path.into(), reason: reason.clone() });
        self.report.errors.push(Issue { path: path.into(), reason });
    }

    fn issue_conflict(&mut self, path: &str, reason: String) {
        (self.progress)(Progress::Conflict { path: path.into(), reason: reason.clone() });
        self.report.conflicts.push(Issue { path: path.into(), reason });
    }
}

/// Download to a temp file, verify quickXorHash (retrying once on mismatch), set mtime, then rename into place.
async fn download_one(graph: &GraphClient, drive: &str, row: &ItemRow, rel: &str, tmp: &Path, root: &Path) -> Result<u64> {
    let part = tmp.join(format!("{}.part", sanitize_id(&row.item_id)));
    let mut last = None;
    for attempt in 0..2 {
        let mut stream = graph.download(drive, &row.item_id).await?;
        let mut f = tokio::fs::File::create(&part).await.with_context(|| format!("creating {}", part.display()))?;
        let mut h = QuickXorHash::new();
        let mut n = 0u64;
        while let Some(chunk) = stream.try_next().await.context("download interrupted")? {
            h.update(&chunk);
            n += chunk.len() as u64;
            f.write_all(&chunk).await?;
        }
        f.flush().await?;
        f.sync_all().await?;
        drop(f);
        let got = h.finalize_base64();
        match row.r_quickxor.as_deref() {
            Some(want) if want != got => {
                tracing::warn!(rel, attempt, "hash mismatch after download");
                last = Some(format!("hash mismatch: SharePoint says {want}, downloaded {got}"));
                continue;
            }
            _ => {}
        }
        if let Some(t) = row.r_mtime.as_deref().and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok()) {
            let ft = filetime::FileTime::from_unix_time(t.timestamp(), t.timestamp_subsec_nanos());
            filetime::set_file_mtime(&part, ft)?;
        }
        let mut dst = root.to_path_buf();
        for seg in rel.split('/') {
            dst.push(seg);
        }
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::rename(&part, &dst).with_context(|| format!("moving download into {}", dst.display()))?;
        return Ok(n);
    }
    let _ = std::fs::remove_file(&part);
    bail!(last.unwrap_or_else(|| "download failed".into()))
}

fn sanitize_id(id: &str) -> String {
    id.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect()
}

fn mtime_ns(md: &std::fs::Metadata) -> i64 {
    md.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

/// Depth-first walk; `visit(rel, is_dir)` returns false to skip a directory's contents.
/// Remove `dir` and every directory under it that is empty, or holds only OS junk files.
/// Is a full listing due? Also when there's no record of one (state from before this was recorded).
fn full_sync_due(last: Option<&str>, now: chrono::DateTime<chrono::Utc>) -> bool {
    match last.and_then(|l| chrono::DateTime::parse_from_rfc3339(l).ok()) {
        Some(t) => now.signed_duration_since(t) >= FULL_SYNC_EVERY,
        None => true,
    }
}

fn remove_empty_dirs(dir: &Path) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            if e.file_type().is_ok_and(|t| t.is_dir()) {
                remove_empty_dirs(&e.path());
            }
        }
    }
    for junk in OS_JUNK {
        let _ = std::fs::remove_file(dir.join(junk));
    }
    let _ = std::fs::remove_dir(dir);
}

fn walk(root: &Path, rel: &str, visit: &mut dyn FnMut(&str, bool) -> bool) -> Result<()> {
    let dir = if rel.is_empty() { root.to_path_buf() } else { root.join(rel) };
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("listing {}", dir.display())),
    };
    for e in entries {
        let e = e?;
        let name = e.file_name().to_string_lossy().into_owned();
        let child = if rel.is_empty() { name } else { format!("{rel}/{name}") };
        let is_dir = e.file_type()?.is_dir();
        if visit(&child, is_dir) && is_dir {
            walk(root, &child, visit)?;
        }
    }
    Ok(())
}

#[cfg(windows)]
fn hide(p: &Path) {
    use std::os::windows::ffi::OsStrExt;
    let wide: Vec<u16> = p.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    unsafe extern "system" {
        fn SetFileAttributesW(name: *const u16, attrs: u32) -> i32;
    }
    unsafe {
        SetFileAttributesW(wide.as_ptr(), 0x2);
    }
}

#[cfg(not(windows))]
fn hide(_: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_sync_daily() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-10-13T12:00:00Z").unwrap().to_utc();
        assert!(!full_sync_due(Some("2026-10-13T00:00:00+00:00"), now));
        assert!(full_sync_due(Some("2026-10-12T11:00:00Z"), now));
        assert!(full_sync_due(None, now));
        assert!(full_sync_due(Some("garbage"), now));
    }
}
