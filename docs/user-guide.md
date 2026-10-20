# looper-sharepoint user guide

This guide covers how to set up, run and understand looper-sharepoint on macOS. The status is a spike, macOS only;
Windows is not yet validated. For a short overview see the [README](../README.md). Other guides:

- Setting up the tenant (an IT admin task): [admin-setup.md](admin-setup.md).
- Embedding the engine in your own app: [embedding.md](embedding.md).

## 1. What it does

looper-sharepoint keeps a local folder in sync with a SharePoint **document library**: a top-level file area inside a
SharePoint site. A Team's "Files" tab is its site's `Documents` library. Each library you configure gets its own
folder under `sync.root`.

- **Two-way by default.**
  - Changes in SharePoint arrive locally within about 30 s (see [Live updates](#5-live-updates)).
  - Local changes reach SharePoint within about 3–5 s.
- **A plain folder.** Every file is a real local file, so any tool can use it: editors, `git`, `grep`, scripts.
  There are no on-demand placeholders yet.
- **One host per machine.** One process does the syncing:
  - `looper-sharepoint run` in a terminal, or
  - the menu-bar app `looper-sharepoint-tray`, or
  - an app that embeds the engine.

  Other commands (`status`, `sync`, `trigger`) talk to whichever host is running.

## 2. Setting up

1. **Install or build:**
   - **From the `.dmg`:** drag Looper SharePoint to Applications (without admin rights: to `~/Applications`), and open
     it from there. Then choose **Install Command Line Tool…** in its menu, so `looper-sharepoint` works in Terminal
     (§2.1).
   - **From source:** `cargo build --release` gives `target/release/looper-sharepoint`. The tray app builds with
     `cargo build --release -p looper-sharepoint-tray`.
2. **Create the config:**
   - `looper-sharepoint config path` shows where it lives. On macOS that's
     `~/Library/Application Support/looper-sharepoint/config.toml`.
   - Start from [config.example.toml](../config.example.toml), and fill in `account.tenant` and `account.client_id`
     from your admin.
3. **Sign in:** `looper-sharepoint login` opens your browser. Use `--device-code` on a machine without one.
   - The sign-in is kept in the macOS keychain.
   - `whoami` shows the account; `logout` forgets it.
4. **Pick libraries:** `looper-sharepoint libraries` lists the sites and libraries you can access, as ready-to-paste
   `[[library]]` blocks. Paste the ones you want into the config.
5. **Check:** `looper-sharepoint config check` validates the file and resolves every library in SharePoint.
6. **Preview, then sync:**
   - `looper-sharepoint sync --dry-run` shows what would happen and changes nothing.
   - `looper-sharepoint sync` does a first full sync.
7. **Keep it running:** use `looper-sharepoint run` or the tray app (§4).

A minimal config:

```toml
[account]
tenant    = "contoso.onmicrosoft.com"
client_id = "00000000-0000-0000-0000-000000000000"

[sync]
root = "~/SharePoint"

[[library]]
site    = "https://contoso.sharepoint.com/sites/Engineering"
library = "Documents"
local   = "engineering"          # → ~/SharePoint/engineering
```

### 2.1 The `looper-sharepoint` command in Terminal

The app contains the CLI, and puts it on your PATH as a link, so app updates carry over.

| | Tray menu | Terminal |
|---|---|---|
| **Install** | Install Command Line Tool…, then "For all users" (`/usr/local/bin`, asks for an administrator password) or "Just for me" (`~/.local/bin`) | `"/Applications/Looper SharePoint.app/Contents/MacOS/looper-sharepoint" install-cli [--user \| --system] [--add-to-path]` |
| **Remove** | Remove Command Line Tool… | `looper-sharepoint uninstall-cli` |

- **"Just for me"** may need `~/.local/bin` adding to your PATH. The app offers to add one marked line to
  `~/.zprofile` (zsh) or `~/.bash_profile` (bash), or runs `fish_add_path` (fish). It never edits them unasked.
- **Open a new Terminal window** afterwards: windows already open don't see it.
- **It only ever replaces its own link.** Something else already at that path (e.g. from `cargo install`) is left as
  it is, and so is a link to something else unless you add `--force`. If another `looper-sharepoint` comes first on
  your PATH, it says so.
- **Moving the app:** it repairs a "just me" link when it next opens. An "all users" link that needs a password is
  repaired from the menu (Repair Command Line Tool…), never at launch.
- **Deleting the app:** choose Remove Command Line Tool… first. Otherwise the link is left pointing at nothing, which
  is harmless.
- It refuses to install from the mounted `.dmg`, or from a copy macOS runs from Downloads: move the app to
  Applications first.

## 3. Configuration reference

Unknown keys are an error, so a typo never silently falls back to a default.

### `[sync]`

| Key | Default | Meaning |
|---|---|---|
| `root` | `~/SharePoint` | Parent folder for libraries given as `local` |
| `direction` | `two_way` | `download_only` makes a mirror: local changes are never uploaded, and local edits and deletes are restored from SharePoint |
| `watch` | `true` | Two-way: upload local changes about 2 s after they settle. If false, they're picked up at the next check |
| `ignore` | `[]` | Extra glob patterns for files never uploaded (see §6) |
| `push` | `true` | Live updates from SharePoint (§5). If false, poll every `poll_interval` |
| `poll_interval` | `45s` | How often to check SharePoint when live updates are off or down. Minimum 15 s |
| `push_safety_interval` | `15m` | While live, still check this often in case a notification is missed. Minimum 1 min |
| `on_remote_delete` | `delete` | What happens to the local copy when a file is deleted in SharePoint: `delete` or `keep`. Deleted files can be recovered from the site's recycle bin |
| `local_orphans` | `keep` | Download-only: what to do with files that exist only locally, `keep` or `delete` |
| `max_delete_fraction` | `0.25` | Refuse a sync that would delete more than this share of a library's files, in either direction. Applies only when at least 10 files would go |
| `max_parallel_downloads` | `4` | Parallel transfers, both downloads and uploads |
| `max_requests_per_second` | `8` | Client-side pacing, below SharePoint's per-user limit. 1–10 |

### `[[library]]`

| Key | Meaning |
|---|---|
| `site` | Site URL, e.g. `https://contoso.sharepoint.com/sites/Engineering` |
| `library` | Library name (as shown in SharePoint) or drive id |
| `path` | The local folder, anywhere (`~/` allowed), e.g. `path = "~/work/specs"` |
| `name` | With `path`: what `--library`, `status`, the tray and the logs call it. Defaults to the last part of `path` (`specs`), with `-2` added if that's taken |
| `local` | The older form, instead of `path`: a folder under `sync.root`, also used as the name |
| `folder` | Optional: sync only this folder of the library, e.g. `folder = "Projects/Specs"`. Its contents go straight into the local folder, so `Projects/Specs/a.docx` is `<path>/a.docx`. It's found by this path once, then followed even if it's renamed or moved in SharePoint |
| `include` / `exclude` | Optional globs on paths inside the library (inside `folder`, when set), e.g. `include = ["specs/**", "*.md"]`. Applied in both directions |
| `read_only` | `true`: never upload to this library, even in two-way mode |

Each library has `path` or `local`. No library's folder may be inside another's, and two libraries can't sync the same
SharePoint folder (or one inside the other's). Two libraries can sync different folders of one SharePoint library.

### `[account]`

| Key | Default | Meaning |
|---|---|---|
| `tenant`, `client_id` | none (required) | Your organization's tenant and app registration, from your admin. The example file's placeholders are refused |
| `app_name` | `looper-sharepoint` | Product name shown on the sign-in page, in `login`/`whoami` and in the tray. Set it to the app registration's display name |
| `publisher` | `looper-sharepoint` | Your organization, sent with the app name in the User-Agent |
| `auth_flow` | `pkce` | `device_code` signs in without a browser on this machine |

### `[network]`

| Key | Default | Meaning |
|---|---|---|
| `min_tls` | `"1.3"` | Minimum TLS version for every connection. `"1.2"` only for a TLS-inspecting proxy that can't do 1.3 |

### `[naming]`

Some names that SharePoint allows can't be stored locally. These are mapped to look-alike Unicode characters, and
mapped back on upload. `mode = "skip"` doesn't sync them instead. `[naming.char_map]` overrides the mapping.

## 4. Running it

### In a terminal: `looper-sharepoint run`

- **Mode bar:** one line shows whether changes are arriving **live** or by **polling**, and why.
- **One row per library:** state, live updates, last sync, last failure, and the next scheduled check.
- **Details panel:** where the library syncs from and to, how local changes are handled, and item-level problems.
- **Recent runs.**

| Key | Action |
|---|---|
| `s` / Enter | Sync the selected library now |
| `a` | Sync all libraries now |
| `r` | Reconnect live updates |
| `p` | Pause or resume scheduled syncing (a manual sync still works) |
| `q` | Quit. A sync in progress finishes first; Ctrl-C twice forces it |

The log goes to `~/Library/Application Support/looper-sharepoint/logs/looper-sharepoint.log`. Use `run --no-tui`
for plain log lines instead, e.g. under a process supervisor.

### In the menu bar: `looper-sharepoint-tray`

A cloud icon shows the state:

| Icon | Meaning |
|---|---|
| Filled | Up to date and live |
| Outlined | Polling because live updates are down |
| Dots | Syncing |
| Pause bars | Waiting (paused or throttled) |
| `!` | Needs attention |

- **The menu** has:
  - the headline and the live/polling line;
  - one line per library (click to open its folder);
  - Sync Now, Pause/Resume;
  - Reconnect Live Updates, Sign In… and Allow Deletions and Sync…, each shown when needed;
  - Status…, Open Config File and Quit.
- **The status window** shows the same information as the TUI. It also has buttons for per-library sync, opening
  folders and allowing a refused deletion.
- **Logs:** `~/Library/Application Support/looper-sharepoint/logs/tray.log`.

### From another terminal, while a host runs

```sh
looper-sharepoint status               # mode, per-library state, request usage, recent runs (--json for scripts)
looper-sharepoint sync                 # sync now through the host and wait: the "git pull"
looper-sharepoint trigger              # sync now, don't wait
looper-sharepoint trigger --reconnect  # reconnect live updates now
```

With no host running, `sync` runs a one-off sync in its own process, and `status` shows what the local state
remembers.

## 5. Live updates

SharePoint tells the client when a library changes, over a connection the client opens to Microsoft. There's no
server to run. Microsoft batches these notifications: a change typically arrives locally **15–35 s** after it's made.

| You see | Meaning | Checks SharePoint |
|---|---|---|
| `● LIVE` | Connected | On every notification, plus every `push_safety_interval` |
| `◐ POLLING every 45s · live updates down …: <reason>` | Push is enabled but not connected. It reconnects with backoff | Every `poll_interval` |
| `○ POLLING … live updates off in config` | `push = false` | Every `poll_interval` |

You can always force a check: Sync Now, `looper-sharepoint sync`, or `s`/`a` in the TUI. Details are in
[sharepoint/push.md](sharepoint/push.md).

## 6. How two-way sync behaves

| Situation | What happens |
|---|---|
| New or edited in SharePoint | Downloaded, verified against SharePoint's hash, then put in place |
| Renamed or moved in SharePoint | Renamed or moved locally; not re-downloaded |
| Deleted in SharePoint | Local copy deleted (unless `on_remote_delete = "keep"`). If you had edited it locally, your version is uploaded again instead, under the same name. If that name is still taken by a file you can no longer see (your access to that one file was removed), your version is **moved to `not-uploaded/`** instead, so it doesn't appear next to the restricted file for everyone |
| **A folder disappears from SharePoint** (deleted, or your access to it was removed: the two look the same) | Its local copy is deleted. Anything in it you had edited or added is **moved to `<root>/not-uploaded/<library>/<date time>/`** and reported as needing attention. It's never uploaded, so restricted content can't reappear where others can read it |
| **You're given access to something** (or given it back) | SharePoint's change feed doesn't report this, so it appears at the next **daily full listing** (each library is listed in full once a day; losing access is also noticed then at the latest). Until then, an edit in a folder you've lost access to fails with "not found" and is retried; nothing is created |
| **Edited or added where you can only read** | SharePoint refuses the upload. Your copy is **moved to `not-uploaded/`** like above, SharePoint's version is put back, and it's reported as needing attention |
| New file or folder locally | Created in SharePoint |
| Edited locally | Uploaded **in place**, so SharePoint's version history keeps every version |
| Renamed or moved locally | Renamed or moved in SharePoint, keeping the same item and history; not re-uploaded. A folder rename is one request |
| Deleted locally | Deleted in SharePoint, to the recycle bin. If it had been changed in SharePoint meanwhile, it's restored instead |
| **Edited on both sides** | Your version is kept as `name (conflict <computer> <date time>).ext` and uploaded too. SharePoint's version takes the original name. Compare the two, keep what you want, and delete the copy |

These are never uploaded, in addition to `sync.ignore`:

- OS junk: `.DS_Store`, `Thumbs.db`, `desktop.ini`
- Office owner/lock and temp files: `~$*`, `~*.tmp`
- LibreOffice locks: `.~lock.*#`
- vim swap, backup and write-test files: `.*.sw?`, `*~`, `4913`
- emacs autosaves: `#*#`
- symbolic links

Saving from Word, Excel, vim or editors that save via a temp file is recognised as an edit of the same document, so
version history continues.

## 7. Safety, and getting files back

- **Recovery:** every delete looper-sharepoint makes, in either direction, can be undone from SharePoint.
  - Files deleted *in SharePoint* by the client go to the **site recycle bin**: in the site, Settings → Site
    contents → Recycle bin. It keeps them for 93 days.
  - Overwritten content is in **version history**.
- **Nothing is overwritten without a copy:**
  - Uploads only succeed if SharePoint still has the version we last saw.
  - New files never replace an existing one.
  - Both-sides edits become conflict copies.
- **The mass-delete guard:** a sync that would delete more than `max_delete_fraction` of a library (at least 10
  files), in either direction, is **refused before changing anything**. The state shows "refused" and the reason.
  If the deletion is intended, allow it once:
  - `looper-sharepoint sync --library <name> --allow-mass-delete`, or
  - **Allow these deletions** in the tray's status window.
- **A missing or emptied local folder never deletes anything in SharePoint.** If you moved or unmounted it, put it
  back. To stop syncing a library, remove it from the config.
- **Files that exist only locally are never deleted** (except with `local_orphans = "delete"` in download-only mode).

## 8. Who can read or write a folder

`looper-sharepoint access` shows and sets who can open a folder or file. PATH is a local path in a synced library
folder, or `--library NAME PATH` for a path in SharePoint.

| Command | What it does |
|---|---|
| `access show PATH` | Who has access and with what role, and whether it has its own permissions or its parent's |
| `access set PATH --read a@x,b@x --write c@x` | Makes exactly these people the readers and writers. Everyone else loses access, except the **site owners, who always keep it** (so nobody can lock out the site's administrators). Shows the changes and asks first; `--dry-run` only shows them, `--yes` doesn't ask |
| `access set PATH --owners-only` | Only the site owners |
| `access check PERSON PATH` | Whether that person can read or write it. Exact when it's granted to them directly. Through a group it's only "possibly", because group membership isn't readable with this app's permissions; the command links to SharePoint's page, where "Check Permissions" answers it |
| `access check me PATH` | Your own access, tested for real (a write test that's cancelled before anything is written) |
| `access inherit PATH` | Use the parent folder's permissions again. Graph can't do this, so it opens SharePoint's page: choose "Delete unique permissions" |

- **Removing access needs a site owner.** A site member can grant but not remove: `set` then stops with "Access was
  added, nothing was removed".
- **Order:** people are granted first and everything else removed last, so a failure never takes away more than
  intended.
- **No emails** to the people granted access, unless `--notify`.
- **Outside the organisation:** refused unless `--allow-external` (and SharePoint's sharing settings allow it). Give a
  guest's email address; SharePoint lists them by their guest sign-in name (`name_domain#ext#@…`), which `show` prints.
- **The library root** is refused unless `--allow-root`: it would restrict the whole library.
- **Groups:** people only for now.

### SharePoint pages: `looper-sharepoint web`

`looper-sharepoint web PATH [PAGE]` opens a folder or file's page in SharePoint (`--print` prints the link). PAGE
is `item` (the default), `permissions` (advanced permissions: inheritance, groups, Check Permissions), `versions`
(version history), `recycle-bin`, `site-permissions`, `library-settings` or `site`.

## 9. Troubleshooting

| Symptom | What to do |
|---|---|
| "Sign-in required" | Run `looper-sharepoint login`, or Sign In… in the tray. Syncing resumes on its own |
| "SharePoint is throttling requests; waiting until …" | Nothing to do. Every request pauses until then, as SharePoint asks. Persistent throttling with many devices: see [sharepoint/throttling.md](sharepoint/throttling.md) |
| Refused (mass delete) | See §7 |
| `◐ POLLING … live updates down` for a long time | Check the network. Then use Reconnect Live Updates, `r`, or `trigger --reconnect`. Syncing still works, just slower |
| A conflict copy appeared | Both sides changed the file. Compare, keep what you want, delete the copy (§6) |
| "already running (pid N)" | Another host has the data folder. Quit it, or use that host via `status` / `sync` |
| Something else | `looper-sharepoint status` shows the last failure per library. The logs are in `~/Library/Application Support/looper-sharepoint/logs/`, and `-v` / `-vv` give more detail |

All state (one SQLite database per library, run history, logs) lives in `~/Library/Application Support/looper-sharepoint/`,
never in the synced folder. `LOOPER_SHAREPOINT_DATA_DIR` moves it.

## 10. What it costs SharePoint

Every device signed in through the same app registration shares one request budget per tenant. One library costs
about:

| Mode | Cost per day |
|---|---|
| Idle, live, quiet library | ~190 RU |
| Each change anywhere in the library | ~1 RU more per live device |
| Idle, polling every 45 s | ~1,870 RU |
| Each change you download | ~1.5 RU |
| Each change you upload | ~5 RU |

The budget for a ≤1,000-license tenant is 1.2M RU/day. `looper-sharepoint status` shows the running estimate. See
[spike-report.md](spike-report.md) for the measurements and [sharepoint/throttling.md](sharepoint/throttling.md) for
the limits.
