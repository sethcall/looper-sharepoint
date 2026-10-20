# M4 Design: two-way sync (local → SharePoint)

Status: **implemented, pending sign-off** (§11 has the results and where the build differs from this plan). Builds on `specs/m1-design.md` (the state DB, base state and the remote-driven engine), M2's service and
M3's live updates. Closes spike-plan criterion 6.

## 1. Goal

Local changes reach SharePoint: new, edited, renamed, moved and deleted files and folders. Nothing is lost when both
sides change.

This is the first milestone where the client can damage data on the server, so the safety rules in §4 come before
convenience.

### Exit criteria

- **Criterion 6:**
  - New, modified and deleted local files upload.
  - Uploads are conditional (`If-Match`).
  - A conflicting edit produces a conflict copy, not data loss.
- **Renames and moves:** local renames and moves of files and folders move the same SharePoint item. They keep its
  id and version history, and nothing is re-uploaded.
- **Safety refusals:**
  - A local mass delete is refused before anything is deleted in SharePoint.
  - A missing local library folder never deletes anything.
  - `read_only` libraries and `direction = "download_only"` never upload.
- **Echoes:** our own uploads don't come back as downloads or conflicts, whether they arrive through delta or push.
- **Latency:** a local edit is in SharePoint within about 5 s (watcher debounce plus upload), in a host with the
  watcher on.

## 2. Config

| Key | Default | Notes |
|---|---|---|
| `sync.direction` | **`two_way`** (was `download_only`) | `download_only` keeps M1 behaviour exactly. Policy-lockable later |
| `[[library]] read_only` | `false` | Per library: behaves as `download_only` |
| `sync.watch` | `true` | Watch library folders and sync about 2 s after local changes stop. Off: local changes are picked up by the next scheduled run |
| `sync.ignore` | see below | Extra glob patterns for local files that are never uploaded, added to the defaults |

- **Built-in ignore list**, always applied to uploads:
  - OS junk: `.DS_Store`, `Thumbs.db`, `desktop.ini`
  - Office owner files and save temp files: `~$*`, `~*.tmp`
  - Editors: vim swap files (`.*.sw[a-p]`), vim's write-test file `4913`, backups `*~`, emacs autosaves `#*#`
  - LibreOffice locks: `.~lock.*#`
  - Our temp folder: `.looper-sharepoint-tmp`
- **Ignored files are left alone** and not reported as orphans.
- **Filters apply in both directions:** a file excluded by `include` / `exclude` isn't uploaded. It's reported as
  "local only (outside this library's filter)".

**Default change:** two-way becomes the default. That's the product intent (`specs/policy.md` lists `two_way` as the
default); admins who want a mirror set `download_only`. Existing configs that say `direction = "download_only"` keep
working.

## 3. How a two-way run works

The same single pass as today, extended. Every run already stats every tracked file and walks the folder, so local
changes are found by comparing against the base (the `l_*` columns). The watcher only decides *when* a run happens.

1. **Pull delta and stage remote state.** This is unchanged.
2. **Scan local.**
   - Tracked files are compared with their base, as `Same`, `Missing` or `Differs`.
   - Untracked files and folders come from the walk, minus ignored ones, and are hashed.
   - Ignored files are skipped; untracked files outside the filter are reported but not uploaded.
3. **Detect local moves** before anything else (§3.1). Matched moves are applied in SharePoint first, and their rows
   updated, so the rest of the run sees a consistent picture.
4. **Plan every item** with the two-way decision table (§4), a pure function in `looper-sharepoint-core` with unit
   tests for each cell.
5. **Guards, before any change:**
   - the existing remote → local delete guard
   - a new local → remote delete guard
   - the missing-root check (§4.2)
6. **Execute.**
   - The remote → local side first, as today: local deletes, moves, folders, downloads, conflict copies.
   - Then local → remote: create remote folders (shallowest first), upload files, then delete remote files, then
     remote folders (deepest first).
7. **Save the deltaLink from step 1.** Our own changes then show up in the next delta, where they are recognised as
   echoes (§5).

**Transactions:**
- Today a whole run is one SQLite transaction. That's wrong once a run has side effects in SharePoint that can't be
  rolled back.
- From M4, staging and planning stay in a transaction. Dry runs roll it back, as before.
- After planning it commits, and **every executed change commits its own row update**.
- So a crash mid-run leaves the DB matching what was actually done on disk and in SharePoint. This also fixes the M2
  known issue where an interrupted run reported moved files as conflicts.

### 3.1 Local renames and moves

A rename shows up as a tracked file that's `Missing` plus an untracked file. If the untracked file has the same
quickXorHash and size as the missing file's base, it's the same file:

- **Files:** `PATCH /items/{id}` with the new `name` and/or `parentReference` (`conflictBehavior = fail`). No
  re-upload, and the id and version history are kept.
  - If two missing files have identical content, prefer a candidate with the same name.
  - If it's still ambiguous, don't guess: treat them as a delete plus a new file.
- **Folders:** a tracked folder whose directory is missing, where every matched file under it moved to the same new
  prefix with the same relative paths, is moved as one folder `PATCH`. Its children follow, with no per-file
  requests. A partial match falls back to per-file moves.
- **Name taken in SharePoint (409):**
  - Skip the move, report a conflict, and leave both items alone.
  - The rename is retried on the next run, and keeps being reported until the user resolves it.
- **Remote changes:**
  - If the item was also renamed remotely, the local name wins.
  - If its content was changed remotely, the move still happens and the new content downloads to the new path.

## 4. Two-way decisions

`base` is the row's local state as last synced. "Remote changed" means the remote hash differs from the base hash,
falling back to size when no hash is available.

| Remote | Local | Two-way action | Download-only / read-only (M1, unchanged) |
|---|---|---|---|
| unchanged | same | nothing | nothing |
| unchanged | **edited** | **upload in place** (`If-Match: eTag`) | keep edit, report |
| unchanged | **missing** | **delete in SharePoint** (recycle bin), guarded | restore (download) |
| changed | same | download | download |
| changed | missing | restore (download); report "deleted locally but changed in SharePoint" | download |
| changed | edited, same content | adopt | adopt |
| changed | **edited, different** | **conflict copy** (§4.1) | conflict, report |
| deleted | same | delete locally (`on_remote_delete`) | same |
| deleted | **edited** | **re-upload as a new file** at its path; report | keep, conflict |
| deleted | missing | forget | forget |
| new remote, no base | missing | download | download |
| new remote, no base | different file at the path | conflict copy | conflict, report |
| no remote item | **new local file or folder** | **create in SharePoint** (`conflictBehavior = fail`) | report as local only |

### 4.1 Conflict copies

- **Naming:** the local version is renamed to `name (conflict <host> <yyyy-mm-dd hhmm>).ext`, in the same folder.
- **The conflict copy is uploaded as a new file**, so collaborators see it in SharePoint too.
- **The remote version** is then downloaded to the original name.
- **Reporting:**
  - The run reports a conflict naming both files.
  - The conflict stays listed in status until the copy is deleted or renamed.
- **No content is ever overwritten without a copy.**
- **Only `keep_both` exists.** The spike plan's `remote_wins` / `local_wins` options are not in M4.

### 4.2 Safety rules

1. **Deletes go to the recycle bin** (`DELETE /items/{id}`). Never `permanentDelete`.
2. **Content updates are in place** on the existing item id, which preserves version history. Never delete and
   re-create.
3. **Every in-place upload is conditional** on the eTag from the last delta.
   - On 412, nothing is overwritten. The row is marked "changed in SharePoint while uploading" and the next run's
     delta turns it into a normal remote-changed decision, i.e. a conflict copy if the local edit is still there.
4. **New items use `conflictBehavior = fail`.**
   - If an item with that name appeared in SharePoint meanwhile (409), nothing is overwritten.
   - The next delta brings the remote item, and the case becomes "different file at the path" → conflict copy.
5. **Local → remote delete guard:**
   - It uses the same rule as the existing guard: `max_delete_fraction` of tracked files, with a floor of 10.
   - It counts files that would be deleted in SharePoint.
   - It refuses the whole run before any change, with the same `Refused` state and `--allow-mass-delete` override.
   - The tray and status window get an **"Allow these deletions"** action, so a GUI user isn't sent to the CLI.
6. **Missing library folder:** if the library's local folder doesn't exist, or is empty while the DB says files were
   synced, the run fails with "local folder missing" and changes nothing. This covers unmounted drives and a
   moved-away folder; the fraction guard alone wouldn't catch tiny libraries.
7. **Folder deletes:**
   - A remote folder is deleted only when every remote item under it is being deleted by this run.
   - If it contains anything we don't sync (filtered, skipped, unknown), only the tracked files are deleted and the
     folder stays.
8. **Files being written:**
   - A file is hashed, uploaded, then stat'ed again.
   - If it changed during the upload, the new base isn't recorded, so the next run uploads again.
   - The watcher's 2 s quiet period avoids most of these.
9. **Names SharePoint rejects** (Graph 400 `invalidRequest` / `nameAlreadyExists` variants): the item is marked
   skipped with the reason and reported. It isn't retried every run until the file changes.
   - Lookalike-mapped names (M1 `naming`) map back to the original characters on upload (`CharMap::to_remote`).
10. **Symlinks and special files** are not followed or uploaded; they're reported once.

## 5. Echo suppression

Our own changes come back to us through delta and through push notifications.

- **Uploads:**
  - Each upload response (a driveItem with `file.hashes.quickXorHash` and `eTag`) is written into the row as both the
    remote state and the base.
  - When the change arrives in delta it matches the base, so the decision is "nothing". It costs 1 RU for the delta
    page and nothing else.
- **SharePoint rewrites some Office files** after upload, adding column metadata, so its hash then differs.
  - With the local file still at base, that's "remote changed, local same" → download. It's correct, never a
    conflict.
- **Moves, creates and deletes** are recorded from the response in the same way.
- **The watcher ignores our own writes.**
  - The engine records the paths it writes (downloads, moves, conflict renames) with a timestamp.
  - Watcher events for those paths within 3 s are dropped.
  - Anything else during a run schedules one follow-up run, via the existing coalescing.

## 6. Watcher

- **One recursive watcher per library folder**, using the `notify` crate: FSEvents on macOS,
  ReadDirectoryChangesW on Windows.
- **Debounce:** events start a 2 s quiet timer, and a run starts when it expires. While events keep arriving, the run
  starts after at most 10 s.
- **The run uses `Trigger::Local`** (new), and goes through the same per-library queue as every other trigger.
- **If the watcher fails** (e.g. the folder doesn't exist yet), it's retried with each run. Local changes are still
  picked up by scheduled runs, because every run scans.
- **Status:**
  - `LibraryStatus` gains `watching: bool`, so hosts can say "local changes sync immediately" or "local changes sync
    at the next check".
  - Pause also stops watcher-triggered runs.

## 7. Uploads

- **Every non-empty file uses an upload session**, created by item id (edit) or parent id + name (new):
  - `createUploadSession`, with `If-Match` for edits and `conflictBehavior = fail` for new files.
  - `fileSystemInfo.lastModifiedDateTime` set from the local mtime, so other machines get the same mtime.
  - Chunks of 10 MiB (a multiple of 320 KiB) streamed from disk, not read into memory.
  - That's 2 requests for a small file, the same count as a PUT plus a PATCH for mtime, with one code path.
- **Empty files** use a simple `PUT …/content`, since upload sessions need at least one byte.
- **Verification:** the response's quickXorHash must equal the local hash computed while uploading; otherwise it's
  an error and is retried next run.
- **Concurrency:** `max_parallel_downloads` is renamed in docs to "transfers" and governs uploads too. The key name
  is unchanged.
- **Pacing and throttling:** every request, including each chunk, goes through the pacer and throttle gate.

## 8. Status, events, UI

- **`Report`** gains:
  - `uploaded`, `bytes_uploaded`
  - `created_remote` (files and folders)
  - `moved_remote`
  - `deleted_remote` (a list, like `deleted_local`, with paths so the user knows what's in the recycle bin)
  - `conflict_copies`
- **`Summary` and the run line** say e.g. "3 uploaded, 1 moved in SharePoint, 2 deleted in SharePoint".
- **`Progress`** gains `Uploading { done, total, bytes_done, bytes_total }`, `Uploaded`, `DeletedRemote` and
  `MovedRemote`. The syncing state counts both directions.
- **Hosts:**
  - The TUI, tray and status window show upload counts in runs.
  - The tray and status window offer "Allow these deletions" when a run was refused.
  - The tray menu says whether local changes sync immediately (watching) or at the next check.
- **The latency bench** gains a `--direction up` mode. It edits, creates, renames and deletes files in the host's
  local folder, then measures time until SharePoint (via delta from a separate client) shows the change.

## 9. Testing

- **Unit tests** (`looper-sharepoint-core`):
  - every cell of the §4 table
  - move matching, including ambiguity and folder moves
  - conflict-copy naming
  - ignore patterns
  - the missing-root rule
- **Live tests** (`--features live`, QA tenant, each in its own `runs/<id>/` folder):
  1. A new local file → in SharePoint, with a matching hash.
  2. An edit → uploaded in place: same item id, version count +1.
  3. A local delete → gone from the drive (recycle bin).
  4. A local rename → same item id, new name. A folder rename → one PATCH, children keep their ids.
  5. An edit on both sides → a conflict copy on both sides, and the remote version at the original path.
  6. A local delete plus a remote edit → restored.
  7. A remote delete plus a local edit → re-uploaded.
  8. Deleting 20 of 30 local files → refused, nothing deleted.
  9. The local root removed → error, nothing deleted.
  10. `read_only` → nothing uploaded.
  11. A 25 MB file → chunked upload with a matching hash. An empty file works too.
  12. Echo: a run straight after uploads downloads nothing and reports no conflicts.
- **Manual:** the latency bench in `--direction up` mode against the `run` host, with the watcher on.

## 10. Not in M4

- `remote_wins` / `local_wins` conflict policies.
- Files On-Demand placeholders.
- Uploading permissions or metadata (columns).
- Tracking files with open handles. Office's own save-as-temp-then-rename is handled by the move detection plus the
  ignore list.
- Bandwidth limits.
- Windows-specific checks (M5).

## 11. Implementation notes

Where the build differs from the plan above:

- **Dry runs** detect local moves without applying them, and report them as moves. Moves are excluded from the
  delete guard's count, and new local files are reported as uploads.
- **The missing-root check runs before the library folder is (re)created,** and also refuses an *empty* folder
  when files were synced before.
- **"Allow these deletions":**
  - In the tray menu, it opens the status window, where the banner shows the refusal message (what would be deleted)
    next to the button. The user always sees what they're allowing.
  - The TUI shows the CLI command instead of offering a one-key override.
- **Upload hash checks:**
  - If SharePoint's hash for an uploaded file differs from what was sent, and the file is an Office type that
    SharePoint rewrites with metadata (`docx`, `xlsx`, `pptx` and their macro/template variants, `vsdx`), its version
    downloads on the next run.
  - For any other type, the mismatch is reported as an error and both sides are treated as in sync. The local file
    is never overwritten by a copy that may be bad.
- **Folder deletes can't use `If-Match`.**
  - Checked on the QA tenant: a folder's eTag stays the same when files are added or edited inside it, and folders
    have no cTag.
  - So right before deleting a folder, the engine lists its current contents in SharePoint, recursively (2 RU per
    folder level).
  - If anything is there that our delta hasn't shown us, the folder stays and only the known files are deleted, each
    conditionally.
  - The window left is the few hundred milliseconds between that listing and the DELETE.
- **Watcher:**
  - If the folder doesn't exist yet, the watcher is retried after the next run (or 60 s).
  - Pause stops watcher-triggered runs.
  - Own-write suppression records each written path and its parent folders for 3 s.
- **Not done:**
  - Office Online / desktop Office save patterns beyond the ignore list (manual check outstanding).
  - Windows (M5).

### Results (QA tenant, macOS, 2026-10-11)

**Live tests** (`cargo test -p looper-sharepoint --features live -- --test-threads=1`): download-only, two-way and
read-only all pass. The two-way test covers §9 items 1–9, 11 and 12 in one run, in about 33 s:

| # | Check | Result |
|---|---|---|
| 1, 2, 11 | New file, new folder, in-place edit, empty file, 25 MB chunked file | Uploaded. The in-place edit kept its item id. The big file's size and quickXorHash match |
| 12 | Echo | A sync straight after uploading: 0 downloaded, 0 uploaded, 0 conflicts |
| 4 | Local file rename and folder rename | 2 moved in SharePoint, 0 uploaded, 0 deleted. Item ids kept, including a file two levels inside the renamed folder |
| 3 | Local delete | Deleted in SharePoint (recycle bin) |
| 5 | Edited on both sides | Conflict copy locally and in SharePoint, with SharePoint's version at the original name. The next sync settles |
| 6 | Deleted locally, edited remotely | Restored |
| 7 | Deleted remotely, edited locally | Uploaded again |
| 8 | 14 of about 22 files deleted locally | Refused, nothing deleted. With `--allow-mass-delete`, deleted as one folder |
| 9 | Local library folder moved away | Refused ("missing"), nothing deleted |
| 10 | `read_only` | Nothing uploaded; SharePoint unchanged |

**Host with the watcher on** (`run`, push on):

- **Local → SharePoint latency** (`latency-bench --direction up`, 200 random mutations, 2–15 s apart): all 200
  arrived, and the host was Live throughout.

  | | p50 | p95 | max |
  |---|---|---|---|
  | all | 3.2 s | 3.6 s | 4.6 s |
  | create | 3.4 s | 4.2 s | 4.6 s |
  | edit | 3.2 s | 3.5 s | 3.9 s |
  | rename | 2.7 s | 3.0 s | 3.1 s |
  | delete | 2.6 s | 3.6 s | 4.0 s |

  The latency is the 2 s quiet period plus about 1.2 s for the run.
- **Application save patterns** (scripted, against the live host), with the outcome in SharePoint:

  | Pattern | Steps |
  |---|---|
  | Word | `~$` owner file; write `~WRD0000.tmp`; rename the original to `~WRL0001.tmp`; rename the temp to the original; delete |
  | Excel | Extension-less `8A3B2C10` temp file; rename swap |
  | vim | `.name.swp`, `4913`, a `name~` backup, rewrite |
  | Editor atomic save | Write a temp file, rename it over the original |

  - Each document stayed the **same item**, so its version history is kept, and got the new content.
  - **No temp, swap or lock file** reached SharePoint.
  - Office Online and real desktop Office still need a manual check.
- **The mass-delete guard caught the bench's own cleanup.** Deleting the 40-file bench folder in SharePoint left the
  host refusing to delete all 40 local copies, as designed. The bench now finishes its cleanup by asking the host to
  sync with the override.
- **Echo:**
  - The three uploads came back as one push-triggered run with "no changes".
  - A remote edit downloaded once, and triggered no watcher run.
