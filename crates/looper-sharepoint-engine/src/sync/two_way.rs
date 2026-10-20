//! The local → SharePoint half of a two-way run. See `specs/m4-design.md` §3–§7.
//!
//! - [`Sync::check_local_root`]: never act on a missing library folder.
//! - [`Sync::apply_local_moves`]: local renames/moves become SharePoint renames/moves (same item, no re-upload).
//! - [`Sync::count_local_deletes`]: for the guard, before anything changes.
//! - [`Sync::keep_conflict_copy`]: both sides changed; keep the local version under a new name.
//! - [`Sync::push_local`]: new folders and files, in-place uploads, deletes.

use std::collections::{HashMap, HashSet};

use anyhow::{Result, bail};
use futures_util::StreamExt;
use looper_sharepoint_core::naming::local_key;
use looper_sharepoint_core::quickxor::{QuickXorHash, hash_file};
use looper_sharepoint_core::reconcile::{MissingFile, NewFile, conflict_copy_name, folder_moves, match_moves};
use looper_sharepoint_core::state::{ItemRow, ItemState};
use looper_sharepoint_graph::models::DriveItem;
use looper_sharepoint_graph::{GraphClient, GraphError, UploadTarget};

use super::{HIDDEN, Issue, READ_ONLY, Sync, TMP_DIR, Target, mtime_ns, walk};
use crate::types::Progress;

/// SharePoint rewrites these after upload (it embeds column metadata), so their hash legitimately changes.
const REWRITTEN_BY_SHAREPOINT: &[&str] = &["docx", "docm", "dotx", "xlsx", "xlsm", "xltx", "pptx", "pptm", "potx", "vsdx"];

fn parent_of(p: &str) -> &str {
    p.rsplit_once('/').map(|(a, _)| a).unwrap_or("")
}

fn name_of(p: &str) -> &str {
    p.rsplit('/').next().unwrap_or(p)
}

/// One file to upload.
struct Job {
    rel: String,
    /// `Some(item_id, etag)` for an in-place update; `None` for a new file in `parent_id`.
    existing: Option<(String, Option<String>)>,
    parent_id: String,
    name: String,
}

/// Local renames/moves found by [`Sync::detect_local_moves`].
#[derive(Default)]
pub(super) struct LocalMoves {
    /// Whole-folder moves `(from, to)`, shallowest first.
    folders: Vec<(String, String)>,
    /// Every matched file `(item_id, to)`, including those inside moved folders.
    files: Vec<(String, String)>,
}

impl LocalMoves {
    pub(super) fn moved_ids(&self) -> HashSet<String> {
        self.files.iter().map(|(id, _)| id.clone()).collect()
    }
}

/// What [`upload_one`] sent and got back.
struct Uploaded {
    item: DriveItem,
    hash: String,
    size: u64,
    mtime_ns: i64,
}

/// Untracked local entries, from one walk of the library folder.
struct Untracked {
    dirs: Vec<String>,
    files: Vec<(String, u64)>,
}

impl Sync<'_> {
    /// Two-way only: if files were synced before, the library folder must exist and not be empty. Otherwise an
    /// unmounted drive or a moved-away folder would look like "the user deleted everything".
    pub(super) fn check_local_root(&self) -> Result<()> {
        if self.db.materialized_file_count()? == 0 {
            return Ok(());
        }
        let entries = match std::fs::read_dir(&self.root) {
            Ok(e) => e,
            Err(_) => bail!(
                "the local folder {} is missing. Nothing was changed. If you moved it, move it back or update sync.root; \\
                 if you want to stop syncing this library, remove it from the config",
                self.root.display()
            ),
        };
        let any = entries.flatten().any(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            !self.ignore.is_ignored(&n)
        });
        if !any {
            bail!(
                "the local folder {} is empty, but files were synced into it before. Nothing was changed (this protects \\
                 SharePoint from an unmounted or replaced folder). To really delete everything, delete the files in \\
                 SharePoint instead",
                self.root.display()
            );
        }
        Ok(())
    }

    /// The remote path (remote names) for a local relative path, e.g. to check the filter.
    fn remote_rel(&self, local_rel: &str) -> String {
        local_rel.split('/').map(|seg| self.map.to_remote(seg)).collect::<Vec<_>>().join("/")
    }

    /// Would a new local file at `rel` be synced (filter), or is it local-only?
    fn uploadable(&self, rel: &str) -> bool {
        let remote = self.remote_rel(rel);
        let mut a = remote.as_str();
        while let Some((p, _)) = a.rsplit_once('/') {
            if self.filter.excludes_dir(p) {
                return false;
            }
            a = p;
        }
        self.filter.includes_file(&remote)
    }

    /// Walk the library folder for files and folders no row knows about (minus ignored ones and symlinks).
    fn untracked(&mut self) -> Result<Untracked> {
        let known: HashSet<String> = self.db.all()?.into_iter().filter_map(|r| r.local_key).collect();
        let mut out = Untracked { dirs: vec![], files: vec![] };
        let mut links = vec![];
        let root = self.root.clone();
        let ignore = &self.ignore;
        walk(&root, "", &mut |rel, is_dir| {
            if rel == TMP_DIR || ignore.is_ignored(rel) {
                return false;
            }
            let abs = root.join(rel);
            match std::fs::symlink_metadata(&abs) {
                Ok(m) if m.file_type().is_symlink() => {
                    links.push(rel.to_string());
                    return false;
                }
                Ok(m) if !is_dir && !known.contains(&local_key(rel)) && m.is_file() => out.files.push((rel.to_string(), m.len())),
                Ok(_) if is_dir && !known.contains(&local_key(rel)) => out.dirs.push(rel.to_string()),
                _ => {}
            }
            true
        })?;
        for l in links {
            self.report.skipped.push(Issue { path: l, reason: "symbolic links are not synced".into() });
        }
        out.dirs.sort_by_key(|d| d.matches('/').count());
        out.files.sort();
        Ok(out)
    }

    /// The SharePoint folder id for local folder `rel`, creating it (and its parents) in SharePoint if needed.
    async fn ensure_remote_dir(&mut self, rel: &str) -> Result<String> {
        if rel.is_empty() {
            return Ok(self.db.root()?.ok_or_else(|| anyhow::anyhow!("library root not staged"))?.item_id);
        }
        if let Some(r) = self.db.by_local_key(&local_key(rel))?
            && r.is_dir
        {
            return Ok(r.item_id);
        }
        let parent_id = Box::pin(self.ensure_remote_dir(parent_of(rel))).await?;
        let name = self.map.to_remote(name_of(rel));
        let (item, existed) = self.graph.create_folder(&self.drive_id, &parent_id, &name).await?;
        let mut row = self.db.get(&item.id)?.unwrap_or_else(|| ItemRow::new(&item.id, &name, true));
        row.parent_id = Some(parent_id);
        row.remote_name = item.name.clone().unwrap_or(name);
        row.r_etag = item.e_tag.clone();
        row.local_path = Some(rel.to_string());
        row.local_key = Some(local_key(rel));
        row.state = ItemState::Synced;
        row.reason = None;
        self.db.put(&row)?;
        if !existed {
            self.report.created_remote += 1;
            tracing::info!(path = rel, "created folder in SharePoint");
        }
        Ok(item.id)
    }

    /// Rename/move a remote item to local path `to`. Returns the updated item, or `None` if it was reported.
    async fn move_remote(&mut self, row: &ItemRow, to: &str) -> Result<Option<DriveItem>> {
        let parent_id = self.ensure_remote_dir(parent_of(to)).await?;
        let name = self.map.to_remote(name_of(to));
        let new_parent = (row.parent_id.as_deref() != Some(parent_id.as_str())).then_some(parent_id.as_str());
        let new_name = (row.remote_name != name).then_some(name.as_str());
        if new_parent.is_none() && new_name.is_none() {
            return Ok(Some(DriveItem { id: row.item_id.clone(), ..Default::default() }));
        }
        match self.graph.move_item(&self.drive_id, &row.item_id, new_parent, new_name).await {
            Ok(item) => Ok(Some(item)),
            Err(e) if e.is_conflict() => {
                self.issue_conflict(to, "renamed or moved locally, but that name is already taken in SharePoint".into());
                Ok(None)
            }
            Err(e) => {
                self.issue_error(to, format!("renaming/moving in SharePoint: {e}"));
                Ok(None)
            }
        }
    }

    /// Point `row` (and, for a folder, everything under it) at local path `to` after a successful move.
    fn record_move(&mut self, row: &ItemRow, item: &DriveItem, to: &str) -> Result<()> {
        let from = row.local_path.clone().unwrap_or_default();
        for mut r in self.db.subtree(&row.item_id)? {
            if r.item_id == row.item_id {
                if let Some(p) = item.parent_id() {
                    r.parent_id = Some(p.to_string());
                }
                if let Some(n) = &item.name {
                    r.remote_name = n.clone();
                }
                if item.e_tag.is_some() {
                    r.r_etag = item.e_tag.clone();
                }
            }
            if let Some(p) = r.local_path.as_deref()
                && (p == from || p.starts_with(&format!("{from}/")))
            {
                let np = format!("{to}{}", &p[from.len()..]);
                if !r.is_dir
                    && let Ok(md) = std::fs::metadata(self.abs(&np))
                {
                    r.l_mtime_ns = Some(mtime_ns(&md));
                }
                r.local_key = Some(local_key(&np));
                r.local_path = Some(np);
            }
            self.db.put(&r)?;
        }
        self.report.moved_remote += 1;
        (self.progress)(Progress::MovedRemote { from, to: to.to_string() });
        Ok(())
    }

    /// Detect local renames/moves (a tracked file that's gone plus an untracked file with the same content) and apply
    /// them in SharePoint, folders as a whole where possible. Returns whether anything moved.
    pub(super) async fn apply_local_moves(&mut self, rows: &HashMap<String, ItemRow>, deleted: &HashSet<String>) -> Result<bool> {
        let Some(m) = self.detect_local_moves(rows, deleted)? else { return Ok(false) };
        for (from, to) in m.folders {
            let Some(row) = self.db.by_local_key(&local_key(&from))? else { continue };
            if let Some(item) = self.move_remote(&row, &to).await? {
                tracing::info!(from, to, "folder renamed/moved locally; moved in SharePoint");
                self.record_move(&row, &item, &to)?;
            }
        }
        for (id, to) in m.files {
            let Some(row) = self.db.get(&id)? else { continue };
            if row.local_path.as_deref() == Some(to.as_str()) {
                continue; // carried by a folder move
            }
            if let Some(item) = self.move_remote(&row, &to).await? {
                tracing::info!(from = row.local_path.as_deref(), to, "renamed/moved locally; moved in SharePoint");
                self.record_move(&row, &item, &to)?;
            }
        }
        Ok(true)
    }

    /// Local renames/moves, without applying them: whole-folder moves `(from, to)` and file moves `(item_id, to)`
    /// (including files carried by those folder moves).
    pub(super) fn detect_local_moves(&mut self, rows: &HashMap<String, ItemRow>, deleted: &HashSet<String>) -> Result<Option<LocalMoves>> {
        let materialized = |r: &&ItemRow| r.is_materialized() && !deleted.contains(&r.item_id);
        let missing: Vec<MissingFile> = rows
            .values()
            .filter(materialized)
            .filter(|r| !r.is_dir && r.l_quickxor.is_some() && r.l_size.is_some())
            .filter(|r| !self.abs(r.local_path.as_deref().unwrap_or_default()).exists())
            .map(|r| MissingFile {
                item_id: r.item_id.clone(),
                path: r.local_path.clone().unwrap_or_default(),
                quickxor: r.l_quickxor.clone().unwrap_or_default(),
                size: r.l_size.unwrap_or(0),
            })
            .collect();
        if missing.is_empty() {
            return Ok(None);
        }
        // Only files with a matching size can be the same file: hash just those.
        let sizes: HashSet<i64> = missing.iter().map(|m| m.size).collect();
        let untracked = self.untracked()?;
        let mut new = vec![];
        for (rel, size) in &untracked.files {
            if sizes.contains(&(*size as i64)) {
                new.push(NewFile { path: rel.clone(), quickxor: hash_file(&self.abs(rel))?, size: *size as i64 });
            }
        }
        let moves = match_moves(&missing, &new);
        if moves.is_empty() {
            return Ok(None);
        }

        let path_of: HashMap<&str, &str> = missing.iter().map(|m| (m.item_id.as_str(), m.path.as_str())).collect();
        let file_moves: Vec<(String, String)> = moves.iter().map(|(id, to)| (path_of[id.as_str()].to_string(), to.clone())).collect();
        let missing_dirs: Vec<String> = rows
            .values()
            .filter(materialized)
            .filter(|r| r.is_dir && !r.is_root)
            .filter_map(|r| r.local_path.clone())
            .filter(|p| !self.abs(p).is_dir())
            .collect();
        let files_under = |d: &str| -> Vec<String> {
            rows.values()
                .filter(|r| !r.is_dir && r.is_materialized())
                .filter_map(|r| r.local_path.clone())
                .filter(|p| p.starts_with(&format!("{d}/")))
                .collect()
        };
        let folders = folder_moves(&file_moves, &missing_dirs, &files_under);
        Ok(Some(LocalMoves { folders, files: moves }))
    }

    /// Two-way dry run: report local moves, local deletes and new uploads as the real run would do them.
    pub(super) fn describe_local_changes(&mut self, rows: &HashMap<String, ItemRow>, deleted: &HashSet<String>) -> Result<()> {
        let m = self.detect_local_moves(rows, deleted)?.unwrap_or_default();
        let moved_ids: HashSet<&str> = m.files.iter().map(|(id, _)| id.as_str()).collect();
        let new_paths: HashSet<&str> = m.files.iter().map(|(_, to)| to.as_str()).collect();
        let in_folder_move = |p: &str| m.folders.iter().any(|(from, _)| p.starts_with(&format!("{from}/")));
        let path_of = |id: &str| rows.get(id).and_then(|r| r.local_path.clone()).unwrap_or_default();
        self.report.moved_remote = m.folders.len() + m.files.iter().filter(|(id, _)| !in_folder_move(&path_of(id))).count();
        // Moved files aren't deletes (describe_dry_run counted every missing file as one).
        let moved_paths: HashSet<String> = moved_ids.iter().map(|id| path_of(id)).collect();
        self.report.deleted_remote.retain(|p| !moved_paths.contains(p));
        for (rel, _) in self.untracked()?.files {
            if new_paths.contains(rel.as_str()) {
                continue;
            }
            if self.uploadable(&rel) {
                self.report.uploaded += 1;
            } else {
                self.report.local_only.push(rel);
            }
        }
        Ok(())
    }

    /// How many files this run would delete in SharePoint because they're gone locally (for the guard).
    /// `moved` are item ids found to be local moves but not applied yet (dry run).
    pub(super) fn count_local_deletes(
        &self,
        rows: &HashMap<String, ItemRow>,
        deleted: &HashSet<String>,
        targets: &HashMap<String, Target>,
        moved: &HashSet<String>,
    ) -> usize {
        rows.values()
            .filter(|r| !r.is_dir && r.is_materialized() && !deleted.contains(&r.item_id) && !moved.contains(&r.item_id))
            .filter(|r| {
                let Some(Target::Path(p)) = targets.get(&r.item_id) else { return false };
                let cur = r.local_path.as_deref().unwrap_or(p);
                let remote_unchanged = match (&r.r_quickxor, &r.l_quickxor) {
                    (Some(a), Some(b)) => a == b,
                    _ => r.r_size == r.l_size,
                };
                remote_unchanged && !self.abs(cur).exists() && !self.abs(p).exists()
            })
            .count()
    }

    /// Both sides changed `rel`: rename the local file to a conflict copy (uploaded later as a new file) so the remote
    /// version can be downloaded to `rel`. Returns false if the copy couldn't be made (then nothing is overwritten).
    pub(super) fn keep_conflict_copy(&mut self, rel: &str) -> Result<bool> {
        let host = gethostname::gethostname().to_string_lossy().split('.').next().unwrap_or("this computer").to_string();
        let when = chrono::Local::now().format("%Y-%m-%d %H%M").to_string();
        let base = conflict_copy_name(name_of(rel), &host, &when);
        let dir = parent_of(rel);
        let mut copy = if dir.is_empty() { base.clone() } else { format!("{dir}/{base}") };
        let mut n = 2;
        while self.abs(&copy).exists() {
            let b = conflict_copy_name(name_of(rel), &host, &format!("{when} {n}"));
            copy = if dir.is_empty() { b } else { format!("{dir}/{b}") };
            n += 1;
        }
        if let Err(e) = std::fs::rename(self.abs(rel), self.abs(&copy)) {
            self.issue_error(rel, format!("changed on both sides, and keeping a conflict copy failed: {e}"));
            return Ok(false);
        }
        tracing::warn!(path = rel, copy, "changed locally and in SharePoint; kept the local version as a conflict copy");
        (self.progress)(Progress::ConflictCopy { path: rel.to_string(), copy: copy.clone() });
        self.report.conflicts.push(Issue { path: rel.to_string(), reason: format!("changed on both sides; your version was kept as {copy}") });
        self.report.conflict_copies.push(copy);
        Ok(true)
    }

    /// Upload new and edited local files, create new folders, and apply local deletes in SharePoint.
    pub(super) async fn push_local(&mut self) -> Result<()> {
        // 1. New folders and files (including conflict copies and re-uploads made earlier in this run). Upload before
        //    deleting anything, so content always exists in SharePoint first.
        let untracked = self.untracked()?;
        let mut jobs = vec![];
        for d in &untracked.dirs {
            let remote = self.remote_rel(d);
            if self.filter.includes_dir(&remote)
                && let Err(e) = self.ensure_remote_dir(d).await
            {
                self.issue_error(d, format!("creating folder in SharePoint: {e:#}"));
            }
        }
        for (rel, _) in &untracked.files {
            if !self.uploadable(rel) {
                self.report.local_only.push(rel.clone());
                continue;
            }
            match self.ensure_remote_dir(parent_of(rel)).await {
                Ok(parent_id) => {
                    let name = self.map.to_remote(name_of(rel));
                    if self.reuploads.contains(rel) {
                        // Gone from SharePoint, edited here. A real delete frees the name; a file that's only hidden
                        // from you (its access was restricted) keeps it. Claim the name with an empty file first, so
                        // nothing is uploaded where people who weren't meant to see it can.
                        match self.graph.create_empty_file(&self.drive_id, &parent_id, &name).await {
                            Ok(item) if item.name.as_deref() == Some(name.as_str()) => {
                                // From here it's an ordinary edit of the new item, if this upload fails.
                                self.reuploads.remove(rel);
                                self.record_claimed(rel, &item)?;
                                jobs.push(Job { rel: rel.clone(), existing: Some((item.id, item.e_tag)), parent_id, name })
                            }
                            // Renamed, or refused (seen both ways): the name is held by a file hidden from you, or
                            // you can only read the folder. Either way it isn't uploaded.
                            Ok(item) => {
                                self.graph.delete_item(&self.drive_id, &item.id).await?;
                                self.report.local_edits_kept.retain(|p| p != rel);
                                if self.set_aside(rel, HIDDEN)? {
                                    self.reuploads.remove(rel);
                                }
                            }
                            // Refused although you can write the folder: the name is held by a file hidden from
                            // you. Or you can only read the folder. Either way it isn't uploaded. Ask twice, as a 403
                            // can be passing.
                            Err(e) if e.is_forbidden() => {
                                if self.claim_refused_again(&parent_id, &name).await? {
                                    self.report.local_edits_kept.retain(|p| p != rel);
                                    if self.set_aside(rel, HIDDEN)? {
                                        self.reuploads.remove(rel);
                                    }
                                } else {
                                    self.issue_error(rel, "uploading again: SharePoint refused it once (403); retried next run".into());
                                }
                            }
                            Err(e) => self.issue_error(rel, format!("uploading again: {e:#}")),
                        }
                        continue;
                    }
                    jobs.push(Job { rel: rel.clone(), existing: None, parent_id, name })
                }
                Err(e) => self.issue_error(rel, format!("creating its folder in SharePoint: {e:#}")),
            }
        }
        // Forget re-uploads whose file is gone locally; keep the rest (failed attempts) for next run.
        let root = self.root.clone();
        self.reuploads.retain(|r| root.join(r).exists());
        self.save_reuploads()?;
        // 2. Edited files, in place.
        for (id, rel) in std::mem::take(&mut self.uploads) {
            if let Some(row) = self.db.get(&id)? {
                jobs.push(Job { rel, existing: Some((id, row.r_etag.clone())), parent_id: String::new(), name: row.remote_name.clone() });
            }
        }
        self.upload_all(jobs).await?;

        // 3. Deletes: whole folders where everything under them is going, then the remaining files.
        self.delete_remote().await
    }

    async fn upload_all(&mut self, jobs: Vec<Job>) -> Result<()> {
        if jobs.is_empty() {
            return Ok(());
        }
        let total = jobs.len();
        let sizes: HashMap<String, u64> =
            jobs.iter().map(|j| (j.rel.clone(), std::fs::metadata(self.abs(&j.rel)).map(|m| m.len()).unwrap_or(0))).collect();
        let bytes_total: u64 = sizes.values().sum();
        (self.progress)(Progress::Uploading { done: 0, total, bytes_done: 0, bytes_total });
        let graph = self.graph.clone();
        let drive = self.drive_id.clone();
        let root = self.root.clone();
        let mut results = futures_util::stream::iter(jobs)
            .map(|job| {
                let (graph, drive, abs) = (graph.clone(), drive.clone(), {
                    let mut p = root.clone();
                    for seg in job.rel.split('/') {
                        p.push(seg);
                    }
                    p
                });
                async move {
                    let r = upload_one(&graph, &drive, &job, &abs).await;
                    (job, r)
                }
            })
            .buffer_unordered(self.cfg.sync.max_parallel_downloads);
        let mut done = 0;
        let mut refused = vec![];
        while let Some((job, r)) = results.next().await {
            done += 1;
            match r {
                Ok(up) => self.record_upload(&job, up)?,
                Err(e) => match e.downcast_ref::<GraphError>() {
                    // Read-only access: retrying won't help (and an error holds back the delta link).
                    Some(g) if g.is_forbidden() => refused.push(job),
                    Some(g) if g.is_precondition_failed() => {
                        // Changed in SharePoint since our delta: nothing overwritten; the next run sees the remote change
                        // and makes a conflict copy if the local edit is still there.
                        if let Some((id, _)) = &job.existing
                            && let Some(mut row) = self.db.get(id)?
                        {
                            row.state = ItemState::Conflict;
                            row.reason = Some("changed in SharePoint while uploading".into());
                            self.db.put(&row)?;
                        }
                        self.issue_conflict(&job.rel, "changed in SharePoint while uploading; not overwritten (resolved next sync)".into());
                    }
                    Some(g) if g.is_conflict() => {
                        self.issue_conflict(&job.rel, "a file with this name was just created in SharePoint; not overwritten (resolved next sync)".into());
                    }
                    Some(GraphError::Http { status: 400, message, .. }) => {
                        self.report.skipped.push(Issue { path: job.rel.clone(), reason: format!("SharePoint refused the name or file: {message}") });
                    }
                    _ => self.issue_error(&job.rel, format!("uploading: {e:#}")),
                },
            }
            let bytes_done = self.report.bytes_uploaded;
            (self.progress)(Progress::Uploading { done, total, bytes_done, bytes_total });
        }
        drop(results);
        self.set_aside_refused(refused).await
    }

    /// Uploads refused for lack of write access: move the local copy to `not-uploaded/`, and put SharePoint's version
    /// back in place of an edited file.
    async fn set_aside_refused(&mut self, refused: Vec<Job>) -> Result<()> {
        let mut restore = vec![];
        for job in refused {
            // A 403 isn't always lasting (seen once among parallel uploads into a new folder): only set aside when
            // you really can't write the file (or, for a new one, its folder).
            let probe = job.existing.as_ref().map(|(id, _)| id.as_str()).unwrap_or(&job.parent_id).to_string();
            if self.can_write(&probe).await? {
                self.issue_error(&job.rel, "uploading: SharePoint refused it (403), but you can write here; retried next run".into());
                continue;
            }
            if self.set_aside(&job.rel, READ_ONLY)?
                && let Some((id, _)) = &job.existing
                && let Some(row) = self.db.get(id)?
            {
                restore.push((row, job.rel));
            }
        }
        self.download_all(restore).await
    }

    /// Record an upload: the response is both the new remote state and (as uploaded) the new base.
    /// After a refused name claim: is it refused again, a few seconds later? (Got it this time: undone, and the next
    /// run claims it properly.)
    async fn claim_refused_again(&mut self, parent_id: &str, name: &str) -> Result<bool> {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        match self.graph.create_empty_file(&self.drive_id, parent_id, name).await {
            Err(e) if e.is_forbidden() => Ok(true),
            Ok(item) if item.name.as_deref() != Some(name) => {
                self.graph.delete_item(&self.drive_id, &item.id).await?;
                Ok(true)
            }
            Ok(item) => {
                self.graph.delete_item(&self.drive_id, &item.id).await?;
                Ok(false)
            }
            Err(_) => Ok(false),
        }
    }

    /// Can the signed-in user write this item (a file, or into a folder)? Tested with a cancelled upload session.
    async fn can_write(&mut self, item_id: &str) -> Result<bool> {
        match self.graph.item(&self.drive_id, item_id).await {
            Ok(item) => Ok(self.graph.can_write(&self.drive_id, &item).await?),
            Err(GraphError::NotFound(_)) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    /// Track the empty file that claimed a re-upload's name, with an empty base, so a failed content upload is retried
    /// next run as an edit of it (never as a new file).
    fn record_claimed(&mut self, rel: &str, item: &DriveItem) -> Result<()> {
        let mut row = ItemRow::new(&item.id, item.name.as_deref().unwrap_or_default(), false);
        row.parent_id = item.parent_id().map(str::to_string);
        row.r_etag = item.e_tag.clone();
        row.r_quickxor = item.quickxor().map(str::to_string);
        row.r_size = Some(0);
        row.r_mtime = item.mtime().map(str::to_string);
        row.local_path = Some(rel.to_string());
        row.local_key = Some(local_key(rel));
        row.l_size = Some(0);
        row.l_mtime_ns = Some(0);
        row.l_quickxor = row.r_quickxor.clone();
        row.state = ItemState::Synced;
        Ok(self.db.put(&row)?)
    }

    fn record_upload(&mut self, job: &Job, up: Uploaded) -> Result<()> {
        let item = &up.item;
        let remote_hash = item.quickxor().map(str::to_string);
        let mut base_hash = up.hash.clone();
        if let Some(r) = &remote_hash
            && *r != up.hash
        {
            let ext = job.rel.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
            if REWRITTEN_BY_SHAREPOINT.contains(&ext.as_str()) {
                // SharePoint added its metadata; the next run downloads its version (local is still at base).
                tracing::info!(path = job.rel, "SharePoint rewrote the uploaded Office file; its version will download");
            } else {
                // Don't let a bad copy in SharePoint overwrite the good local file: treat both as in sync and report.
                base_hash = r.clone();
                self.issue_error(&job.rel, format!("SharePoint stored different content than was uploaded (hash {r}, sent {}); local file kept", up.hash));
            }
        }
        let mut row = match &job.existing {
            Some((id, _)) => self.db.get(id)?.unwrap_or_else(|| ItemRow::new(&item.id, &job.name, false)),
            None => ItemRow::new(&item.id, &job.name, false),
        };
        row.item_id = item.id.clone();
        if let Some(p) = item.parent_id() {
            row.parent_id = Some(p.to_string());
        } else if job.existing.is_none() {
            row.parent_id = Some(job.parent_id.clone());
        }
        row.remote_name = item.name.clone().unwrap_or_else(|| job.name.clone());
        row.r_etag = item.e_tag.clone();
        row.r_quickxor = remote_hash;
        row.r_size = item.size.or(Some(up.size as i64));
        row.r_mtime = item.mtime().map(str::to_string);
        row.local_path = Some(job.rel.clone());
        row.local_key = Some(local_key(&job.rel));
        row.l_size = Some(up.size as i64);
        row.l_mtime_ns = Some(up.mtime_ns);
        row.l_quickxor = Some(base_hash);
        row.state = ItemState::Synced;
        row.reason = None;
        self.db.put(&row)?;
        self.report.uploaded += 1;
        self.report.bytes_uploaded += up.size;
        tracing::info!(path = job.rel, new = job.existing.is_none(), "uploaded");
        (self.progress)(Progress::Uploaded { path: job.rel.clone() });
        Ok(())
    }

    /// Apply local deletes in SharePoint (recycle bin). A folder is deleted as a whole only when everything in it is
    /// being deleted and nothing in it is outside what we sync; otherwise just its files.
    async fn delete_remote(&mut self) -> Result<()> {
        let mut files: HashMap<String, String> = std::mem::take(&mut self.remote_deletes).into_iter().collect();
        let mut dirs: Vec<ItemRow> = vec![];
        for id in std::mem::take(&mut self.missing_dirs) {
            if let Some(r) = self.db.get(&id)? {
                dirs.push(r);
            }
        }
        dirs.sort_by_key(|r| r.local_path.as_deref().map(|p| p.matches('/').count()).unwrap_or(0));
        let mut gone: Vec<String> = vec![]; // folders deleted as a whole (local paths)
        for d in dirs {
            let path = d.local_path.clone().unwrap_or_default();
            if gone.iter().any(|g| path.starts_with(&format!("{g}/"))) {
                continue;
            }
            let sub = self.db.subtree(&d.item_id)?;
            let whole = sub.iter().filter(|r| r.item_id != d.item_id).all(|r| {
                if r.is_dir {
                    r.is_materialized() && r.local_path.as_deref().is_some_and(|p| !self.abs(p).exists())
                } else {
                    files.contains_key(&r.item_id)
                }
            });
            if !whole {
                continue;
            }
            // A folder's eTag doesn't change when files are added inside it, so `If-Match` can't protect a folder
            // delete. Instead, list what's in SharePoint right now: anything we haven't seen (added since our delta)
            // keeps the folder, and its known files are deleted one by one (conditionally) below.
            let known: HashSet<String> = sub.iter().map(|r| r.item_id.clone()).collect();
            match has_unknown_children(self.graph, &self.drive_id, &d.item_id, &known).await {
                Ok(false) => {}
                Ok(true) => {
                    tracing::info!(path, "folder deleted locally, but SharePoint has new items in it; deleting only the known files");
                    continue;
                }
                Err(e) => {
                    self.issue_error(&path, format!("checking folder contents in SharePoint: {e}"));
                    continue;
                }
            }
            match self.graph.delete_item_if(&self.drive_id, &d.item_id, None).await {
                Ok(()) => {
                    tracing::info!(path, "folder deleted locally; deleted in SharePoint (recoverable from the site recycle bin)");
                    for r in &sub {
                        files.remove(&r.item_id);
                        self.db.remove(&r.item_id)?;
                    }
                    (self.progress)(Progress::DeletedRemote { path: format!("{path}/") });
                    self.report.deleted_remote.push(format!("{path}/"));
                    gone.push(path);
                }
                Err(e) => self.issue_error(&path, format!("deleting folder in SharePoint: {e}")),
            }
        }
        let mut files: Vec<(String, String)> = files.into_iter().collect();
        files.sort_by(|a, b| a.1.cmp(&b.1));
        for (id, path) in files {
            let Some(row) = self.db.get(&id)? else { continue };
            match self.graph.delete_item_if(&self.drive_id, &id, row.r_etag.as_deref()).await {
                Ok(()) => {
                    tracing::info!(path, item = id, "deleted locally; deleted in SharePoint (recoverable from the site recycle bin)");
                    self.db.remove(&id)?;
                    (self.progress)(Progress::DeletedRemote { path: path.clone() });
                    self.report.deleted_remote.push(path);
                }
                Err(e) if e.is_precondition_failed() => {
                    self.issue_conflict(&path, "deleted locally but changed in SharePoint; not deleted (it will be restored)".into());
                }
                Err(e) => self.issue_error(&path, format!("deleting in SharePoint: {e}")),
            }
        }
        Ok(())
    }
}

/// Does the SharePoint folder `id` (recursively) contain anything not in `known`?
async fn has_unknown_children(graph: &GraphClient, drive: &str, id: &str, known: &HashSet<String>) -> Result<bool> {
    let mut stack = vec![id.to_string()];
    while let Some(folder) = stack.pop() {
        for c in graph.children(drive, &folder).await? {
            if !known.contains(&c.id) {
                return Ok(true);
            }
            if c.is_dir() {
                stack.push(c.id);
            }
        }
    }
    Ok(false)
}

/// Upload one file, hashing exactly the bytes sent. The file's stat is taken first; if the file changes during the
/// upload, the recorded base (stat + hash of what was sent) no longer matches it, so the next run uploads again.
async fn upload_one(graph: &GraphClient, drive: &str, job: &Job, abs: &std::path::Path) -> Result<Uploaded> {
    let md = std::fs::metadata(abs)?;
    let modified = md.modified().ok().map(|t| chrono::DateTime::<chrono::Utc>::from(t).to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
    let target = match &job.existing {
        Some((id, _)) => UploadTarget::Existing { item_id: id },
        None => UploadTarget::New { parent_id: &job.parent_id, name: &job.name },
    };
    let if_match = job.existing.as_ref().and_then(|(_, e)| e.as_deref());
    let mut h = QuickXorHash::new();
    let mut sent = 0u64;
    let item = graph
        .upload_file(drive, &target, abs, modified.as_deref(), if_match, &mut |b: &[u8]| {
            h.update(b);
            sent += b.len() as u64;
        })
        .await?;
    Ok(Uploaded { item, hash: h.finalize_base64(), size: sent, mtime_ns: mtime_ns(&md) })
}
