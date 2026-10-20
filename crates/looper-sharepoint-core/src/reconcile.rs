//! Pure download-only decisions. See the table in `specs/m1-design.md` §3.
//!
//! No I/O here: the engine stats/hashes the local file and passes the result in, so every row of the table is unit-tested.

use crate::config::OnRemoteDelete;
use crate::state::ItemRow;

/// Remote state of a file, from delta.
#[derive(Debug, Clone)]
pub struct RemoteFile {
    pub quickxor: Option<String>,
    pub size: i64,
}

/// Local file compared with the base recorded in the state DB.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Local {
    /// No file at the item's local path.
    Missing,
    /// File matches the base (what we last wrote or verified).
    Same,
    /// File exists and does not match the base (or there is no base). Carries its quickXorHash.
    Differs { quickxor: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileDecision {
    /// Fetch remote content and write it locally.
    Download,
    /// Local file already has the remote content; record it as base without downloading.
    Adopt,
    /// Nothing to do (metadata such as eTag may still be refreshed).
    Nothing,
    /// Local was edited and remote didn't change. Download-only never uploads, so leave it.
    KeepLocalEdit,
    /// Don't overwrite; report.
    Conflict(&'static str),
}

/// `base` is the existing DB row if the item has been materialized locally before.
pub fn decide_file(base: Option<&ItemRow>, remote: &RemoteFile, local: &Local) -> FileDecision {
    let matches_remote = |h: &str| remote.quickxor.as_deref() == Some(h);
    let Some(base) = base.filter(|b| b.is_materialized() && b.l_size.is_some()) else {
        return match local {
            Local::Missing | Local::Same => FileDecision::Download,
            Local::Differs { quickxor } if matches_remote(quickxor) => FileDecision::Adopt,
            Local::Differs { .. } => FileDecision::Conflict("a different local file already exists at this path"),
        };
    };

    let remote_changed = match (&remote.quickxor, &base.l_quickxor) {
        (Some(r), Some(l)) => r != l,
        // No hash available: fall back to size.
        _ => Some(remote.size) != base.l_size,
    };

    match (remote_changed, local) {
        (_, Local::Differs { quickxor }) if matches_remote(quickxor) => FileDecision::Adopt,
        (false, Local::Same) => FileDecision::Nothing,
        // Deleted locally: download-only mirrors the remote, so restore it.
        (_, Local::Missing) => FileDecision::Download,
        (false, Local::Differs { .. }) => FileDecision::KeepLocalEdit,
        (true, Local::Same) => FileDecision::Download,
        (true, Local::Differs { .. }) => FileDecision::Conflict("changed both locally and in SharePoint"),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeleteDecision {
    /// Remove the local file (recoverable from the SharePoint recycle bin).
    DeleteLocal,
    /// Stop tracking; nothing local to touch.
    Forget,
    /// Stop tracking but leave the local file in place (`on_remote_delete = "keep"`).
    ForgetKeepFile,
    /// Local copy has edits that exist only locally; never silently destroyed.
    KeepModified,
    /// Two-way: the local copy has edits; upload it again as a new file.
    Reupload,
    /// Two-way: the local copy has edits, but its folder is gone from SharePoint too (deleted, or this user lost
    /// access to it). Move it out of the library folder and don't upload it: uploading would recreate the folder,
    /// and restricted content could end up where others can read it.
    SetAside,
}

/// A previously synced file was deleted in SharePoint.
pub fn decide_remote_delete(base: &ItemRow, local: &Local, policy: OnRemoteDelete) -> DeleteDecision {
    if !base.is_materialized() {
        return DeleteDecision::Forget;
    }
    match local {
        Local::Missing => DeleteDecision::Forget,
        Local::Differs { quickxor } if base.l_quickxor.as_deref() != Some(quickxor) => DeleteDecision::KeepModified,
        Local::Same | Local::Differs { .. } => match policy {
            OnRemoteDelete::Delete => DeleteDecision::DeleteLocal,
            OnRemoteDelete::Keep => DeleteDecision::ForgetKeepFile,
        },
    }
}

/// What a two-way run does with a file. See the table in `specs/m4-design.md` §4.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TwoWay {
    Nothing,
    /// Fetch the remote content (remote changed, or new).
    Download,
    /// Local file was deleted but SharePoint has a newer version: bring it back.
    Restore,
    /// Local already has the remote content; record it as base.
    Adopt,
    /// Local edit, remote unchanged: upload in place (conditional on the eTag).
    Upload,
    /// Deleted locally, unchanged remotely: delete in SharePoint (recycle bin).
    DeleteRemote,
    /// Both sides have different content: keep the local version as a conflict copy, then download.
    ConflictCopy,
}

/// Two-way decision for a file that exists remotely. `base` is the existing row if the item was materialized.
pub fn decide_two_way(base: Option<&ItemRow>, remote: &RemoteFile, local: &Local) -> TwoWay {
    let matches_remote = |h: &str| remote.quickxor.as_deref() == Some(h);
    let Some(base) = base.filter(|b| b.is_materialized() && b.l_size.is_some()) else {
        return match local {
            Local::Missing | Local::Same => TwoWay::Download,
            Local::Differs { quickxor } if matches_remote(quickxor) => TwoWay::Adopt,
            Local::Differs { .. } => TwoWay::ConflictCopy,
        };
    };
    let remote_changed = match (&remote.quickxor, &base.l_quickxor) {
        (Some(r), Some(l)) => r != l,
        _ => Some(remote.size) != base.l_size,
    };
    match (remote_changed, local) {
        (_, Local::Differs { quickxor }) if matches_remote(quickxor) => TwoWay::Adopt,
        (false, Local::Same) => TwoWay::Nothing,
        (false, Local::Missing) => TwoWay::DeleteRemote,
        (false, Local::Differs { .. }) => TwoWay::Upload,
        (true, Local::Same) => TwoWay::Download,
        (true, Local::Missing) => TwoWay::Restore,
        (true, Local::Differs { .. }) => TwoWay::ConflictCopy,
    }
}

/// A previously synced file was deleted in SharePoint (two-way): like [`decide_remote_delete`], except that local
/// edits are re-uploaded as a new file instead of only being kept, as long as the file's folder is still there
/// (`folder_gone` false). A file and its folder disappearing together is also what losing access looks like, so then
/// the edits are set aside instead.
pub fn decide_remote_delete_two_way(base: &ItemRow, local: &Local, policy: OnRemoteDelete, folder_gone: bool) -> DeleteDecision {
    match decide_remote_delete(base, local, policy) {
        DeleteDecision::KeepModified if folder_gone => DeleteDecision::SetAside,
        DeleteDecision::KeepModified => DeleteDecision::Reupload,
        d => d,
    }
}

/// A tracked file whose local copy is missing, as a candidate for a local rename/move.
#[derive(Debug, Clone)]
pub struct MissingFile {
    pub item_id: String,
    pub path: String,
    pub quickxor: String,
    pub size: i64,
}

/// An untracked local file.
#[derive(Debug, Clone)]
pub struct NewFile {
    pub path: String,
    pub quickxor: String,
    pub size: i64,
}

fn file_name(p: &str) -> &str {
    p.rsplit('/').next().unwrap_or(p)
}

/// Pair missing tracked files with new local files that have the same content: those are local renames/moves.
/// Returns `(item_id, new_path)`. When content is ambiguous (several candidates on either side), a pair is made only
/// if exactly one candidate has the same file name; otherwise nothing is matched (delete + new is safe, a wrong move
/// is not).
pub fn match_moves(missing: &[MissingFile], new: &[NewFile]) -> Vec<(String, String)> {
    use std::collections::HashMap;
    type Candidates<'a> = (Vec<&'a MissingFile>, Vec<&'a NewFile>);
    let mut by_content: HashMap<(&str, i64), Candidates> = HashMap::new();
    for m in missing {
        by_content.entry((m.quickxor.as_str(), m.size)).or_default().0.push(m);
    }
    for n in new {
        if let Some(e) = by_content.get_mut(&(n.quickxor.as_str(), n.size)) {
            e.1.push(n);
        }
    }
    let mut out = vec![];
    for (ms, ns) in by_content.values() {
        match (ms.as_slice(), ns.as_slice()) {
            (_, []) => {}
            ([m], [n]) => out.push((m.item_id.clone(), n.path.clone())),
            _ => {
                // Ambiguous: only same-name pairs where the name is unique on both sides.
                for m in ms {
                    let same_m = ms.iter().filter(|o| file_name(&o.path) == file_name(&m.path)).count();
                    let cands: Vec<_> = ns.iter().filter(|n| file_name(&n.path) == file_name(&m.path)).collect();
                    if same_m == 1 && cands.len() == 1 {
                        out.push((m.item_id.clone(), cands[0].path.clone()));
                    }
                }
            }
        }
    }
    out.sort();
    out
}

/// Given file moves `(old_path, new_path)` and the paths of tracked folders that are missing locally, find folders
/// that moved as a whole: every matched file under the folder moved to the same new prefix with the same relative path,
/// and `files_under` (all tracked files under it) all moved. Returns `(old_folder, new_folder)`, shallowest first.
pub fn folder_moves(
    file_moves: &[(String, String)],
    missing_dirs: &[String],
    files_under: &dyn Fn(&str) -> Vec<String>,
) -> Vec<(String, String)> {
    use std::collections::HashMap;
    let moved: HashMap<&str, &str> = file_moves.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
    let mut dirs: Vec<&String> = missing_dirs.iter().collect();
    dirs.sort_by_key(|d| d.matches('/').count());
    let mut out: Vec<(String, String)> = vec![];
    for d in dirs {
        // Inside a folder we already move as a whole: it travels with its parent.
        if out.iter().any(|(o, _)| d.starts_with(&format!("{o}/"))) {
            continue;
        }
        let files = files_under(d);
        if files.is_empty() {
            continue;
        }
        let mut target: Option<String> = None;
        let ok = files.iter().all(|f| {
            let Some(to) = moved.get(f.as_str()) else { return false };
            let rel = &f[d.len()..]; // "/sub/x.txt"
            let Some(prefix) = to.strip_suffix(rel) else { return false };
            match &target {
                None => {
                    target = Some(prefix.to_string());
                    true
                }
                Some(t) => t == prefix,
            }
        });
        if ok && let Some(t) = target.filter(|t| !t.is_empty() && t != d) {
            out.push((d.clone(), t));
        }
    }
    out
}

/// `report.docx` → `report (conflict <host> 2026-10-11 1432).docx`. Dotfiles and extension-less names get the suffix
/// at the end.
pub fn conflict_copy_name(name: &str, host: &str, when: &str) -> String {
    let tag = format!(" (conflict {host} {when})");
    match name.rfind('.') {
        Some(i) if i > 0 => format!("{}{tag}{}", &name[..i], &name[i..]),
        _ => format!("{name}{tag}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::ItemState;

    fn base(hash: &str) -> ItemRow {
        let mut r = ItemRow::new("i", "f.txt", false);
        r.local_path = Some("f.txt".into());
        r.local_key = Some("f.txt".into());
        r.l_size = Some(3);
        r.l_mtime_ns = Some(1);
        r.l_quickxor = Some(hash.into());
        r.r_quickxor = Some(hash.into());
        r.state = ItemState::Synced;
        r
    }
    fn remote(hash: &str) -> RemoteFile {
        RemoteFile { quickxor: Some(hash.into()), size: 3 }
    }
    fn differs(h: &str) -> Local {
        Local::Differs { quickxor: h.into() }
    }
    use FileDecision::*;

    #[test]
    fn new_items() {
        assert_eq!(decide_file(None, &remote("A"), &Local::Missing), Download);
        assert_eq!(decide_file(None, &remote("A"), &differs("A")), Adopt);
        assert!(matches!(decide_file(None, &remote("A"), &differs("B")), Conflict(_)));
    }

    #[test]
    fn remote_unchanged() {
        let b = base("A");
        assert_eq!(decide_file(Some(&b), &remote("A"), &Local::Same), Nothing);
        assert_eq!(decide_file(Some(&b), &remote("A"), &Local::Missing), Download);
        assert_eq!(decide_file(Some(&b), &remote("A"), &differs("B")), KeepLocalEdit);
    }

    #[test]
    fn remote_changed() {
        let b = base("A");
        assert_eq!(decide_file(Some(&b), &remote("B"), &Local::Same), Download);
        assert_eq!(decide_file(Some(&b), &remote("B"), &Local::Missing), Download);
        assert_eq!(decide_file(Some(&b), &remote("B"), &differs("B")), Adopt, "same edit made on both sides");
        assert!(matches!(decide_file(Some(&b), &remote("B"), &differs("C")), Conflict(_)));
    }

    #[test]
    fn no_hash_falls_back_to_size() {
        let b = base("A");
        let r = RemoteFile { quickxor: None, size: 3 };
        assert_eq!(decide_file(Some(&b), &r, &Local::Same), Nothing);
        let r = RemoteFile { quickxor: None, size: 4 };
        assert_eq!(decide_file(Some(&b), &r, &Local::Same), Download);
    }

    #[test]
    fn remote_deletes() {
        use DeleteDecision::*;
        let b = base("A");
        assert_eq!(decide_remote_delete(&b, &Local::Same, OnRemoteDelete::Delete), DeleteLocal);
        assert_eq!(decide_remote_delete(&b, &Local::Same, OnRemoteDelete::Keep), ForgetKeepFile);
        assert_eq!(decide_remote_delete(&b, &Local::Missing, OnRemoteDelete::Delete), Forget);
        assert_eq!(decide_remote_delete(&b, &differs("B"), OnRemoteDelete::Delete), KeepModified);
        let mut unmat = b.clone();
        unmat.local_path = None;
        assert_eq!(decide_remote_delete(&unmat, &Local::Same, OnRemoteDelete::Delete), Forget);
    }

    #[test]
    fn two_way_table() {
        use TwoWay as T;
        let b = base("A");
        // remote unchanged
        assert_eq!(decide_two_way(Some(&b), &remote("A"), &Local::Same), T::Nothing);
        assert_eq!(decide_two_way(Some(&b), &remote("A"), &differs("B")), T::Upload);
        assert_eq!(decide_two_way(Some(&b), &remote("A"), &Local::Missing), T::DeleteRemote);
        // remote changed
        assert_eq!(decide_two_way(Some(&b), &remote("B"), &Local::Same), T::Download);
        assert_eq!(decide_two_way(Some(&b), &remote("B"), &Local::Missing), T::Restore);
        assert_eq!(decide_two_way(Some(&b), &remote("B"), &differs("B")), T::Adopt);
        assert_eq!(decide_two_way(Some(&b), &remote("B"), &differs("C")), T::ConflictCopy);
        // new remote item
        assert_eq!(decide_two_way(None, &remote("A"), &Local::Missing), T::Download);
        assert_eq!(decide_two_way(None, &remote("A"), &differs("A")), T::Adopt);
        assert_eq!(decide_two_way(None, &remote("A"), &differs("B")), T::ConflictCopy);
        // remote deleted
        use DeleteDecision as D;
        assert_eq!(decide_remote_delete_two_way(&b, &differs("B"), OnRemoteDelete::Delete, false), D::Reupload);
        assert_eq!(decide_remote_delete_two_way(&b, &differs("B"), OnRemoteDelete::Delete, true), D::SetAside);
        assert_eq!(decide_remote_delete_two_way(&b, &Local::Same, OnRemoteDelete::Delete, true), D::DeleteLocal);
        assert_eq!(decide_remote_delete_two_way(&b, &Local::Missing, OnRemoteDelete::Delete, false), D::Forget);
    }

    fn mf(id: &str, path: &str, h: &str) -> MissingFile {
        MissingFile { item_id: id.into(), path: path.into(), quickxor: h.into(), size: 1 }
    }
    fn nf(path: &str, h: &str) -> NewFile {
        NewFile { path: path.into(), quickxor: h.into(), size: 1 }
    }

    #[test]
    fn moves_by_content() {
        let m = vec![mf("1", "a/x.txt", "H1"), mf("2", "a/y.txt", "H2")];
        let n = vec![nf("b/x.txt", "H1"), nf("a/z.txt", "H2"), nf("c/other.txt", "H9")];
        assert_eq!(match_moves(&m, &n), vec![("1".into(), "b/x.txt".into()), ("2".into(), "a/z.txt".into())]);
    }

    #[test]
    fn ambiguous_moves_need_unique_names() {
        // Two identical files both moved: names disambiguate.
        let m = vec![mf("1", "a/x.txt", "H"), mf("2", "a/y.txt", "H")];
        let n = vec![nf("b/x.txt", "H"), nf("b/y.txt", "H")];
        assert_eq!(match_moves(&m, &n), vec![("1".into(), "b/x.txt".into()), ("2".into(), "b/y.txt".into())]);
        // Renamed too: no safe pairing, so no moves (delete + upload instead).
        let n = vec![nf("b/p.txt", "H"), nf("b/q.txt", "H")];
        assert!(match_moves(&m, &n).is_empty());
        // One missing, two new copies with other names: ambiguous.
        let n = vec![nf("b/p.txt", "H"), nf("b/q.txt", "H")];
        assert!(match_moves(&m[..1], &n).is_empty());
    }

    #[test]
    fn whole_folder_moves() {
        let moves = vec![
            ("docs/a.txt".to_string(), "papers/a.txt".to_string()),
            ("docs/sub/b.txt".to_string(), "papers/sub/b.txt".to_string()),
            ("misc/c.txt".to_string(), "elsewhere/c.txt".to_string()),
        ];
        let under = |d: &str| -> Vec<String> {
            match d {
                "docs" => vec!["docs/a.txt".into(), "docs/sub/b.txt".into()],
                "docs/sub" => vec!["docs/sub/b.txt".into()],
                "misc" => vec!["misc/c.txt".into(), "misc/d.txt".into()],
                _ => vec![],
            }
        };
        let dirs = vec!["docs/sub".to_string(), "docs".to_string(), "misc".to_string()];
        // docs → papers as a folder (docs/sub travels with it); misc only partly moved → no folder move.
        assert_eq!(folder_moves(&moves, &dirs, &under), vec![("docs".to_string(), "papers".to_string())]);
    }

    #[test]
    fn conflict_names() {
        assert_eq!(conflict_copy_name("report.docx", "mac", "2026-10-11 1432"), "report (conflict mac 2026-10-11 1432).docx");
        assert_eq!(conflict_copy_name(".env", "mac", "t"), ".env (conflict mac t)");
        assert_eq!(conflict_copy_name("Makefile", "mac", "t"), "Makefile (conflict mac t)");
        assert_eq!(conflict_copy_name("a.tar.gz", "mac", "t"), "a.tar (conflict mac t).gz");
    }
}
