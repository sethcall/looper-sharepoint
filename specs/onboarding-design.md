# Design: onboarding (setup file and link, `add <link>`, a folder per library, start at login, quickstart)

Status: **in progress** (2026-10-20). Approved with the revisions (`looper-sharepoint://` links, any local folder per
library, a SharePoint folder synced directly into it).

| Step | State |
|---|---|
| Probe: `/shares` for plain links | Done: it resolves folder and library URLs, `AllItems.aspx?id=` pages, file URLs and sharing links to the item. A site URL gives 400 for `driveItem`, but `/shares/{url}/site` works |
| §3 a folder per library, a folder as the root | Built; live test `folder_as_library_root` |
| §5, §6 `add`, `remove`, reload (CLI and `run`) | Built; live tests `add_and_remove_from_links`, `running_host_reloads_after_add` |
| §5, §6 in the tray: Add window, reload, saved config file | Next |
| §4 setup file and link (CLI: `setup`, `setup export`) | Built; offline test `setup_from_link_and_file_and_export`, tried end to end against QA |
| §7 `looper-sharepoint://` and `.looper-sharepoint` opened in the app | With the tray work |
| §8 start at login | |
| §9, §10 help, quickstart, README | |

## 1. Goal

Telling a coworker how to start syncing a SharePoint site should take one message:

> Install the app, click the setup link I sent (or open the file), sign in, then paste the link to the site and
> choose a folder. It keeps itself up to date from then on, including after restarts.

Today the same coworker has to:

- put a hand-written `config.toml` in `~/Library/Application Support`;
- get the tenant id and app id from someone;
- paste `[[library]]` blocks from `libraries`;
- keep every library under one parent folder;
- restart the host after every change;
- remember to start the app again after every restart.

This milestone removes each of those. It also makes `looper-sharepoint --help` complete enough that a person or an
AI assistant can set it up from the help alone (§9).

## 2. The pieces

| Piece | What it replaces |
|---|---|
| **Setup file** (`<name>.looper-sharepoint`) and **setup link** (`looper-sharepoint://setup?…`), both made by `setup export` | Sending the tenant id and app id, and writing `[account]` by hand |
| **`add <SharePoint link> --to <folder>`**, and `looper-sharepoint://add?…` links | `libraries`, then pasting `[[library]]` blocks |
| **A local folder per library**, anywhere (`path`) | One `sync.root` for everything |
| **A SharePoint folder synced directly** into that local folder (`folder`) | Syncing the whole library and filtering with `include` |
| **Config reload** in a running host | Restarting the host after a config change |
| **Start at login** (tray), on by default after the first sync | Starting the app again after every restart |
| **`docs/QUICKSTART.md`**, linked from the README | Reading the whole user guide to start |
| **Help written as instructions** | Guessing the order of commands |

## 3. Config: a folder per library, and a SharePoint folder as its root

```toml
[[library]]
site   = "https://contoso.sharepoint.com/sites/Engineering"
library = "Documents"
folder = "Projects/Specs"          # optional: sync only this folder, its contents directly into `path`
path   = "/foo/bar"                # any folder; `~` allowed
# name = "specs"                   # optional; defaults to the last part of `path` ("bar"), made unique
```

- **`path`:** the library's local folder, absolute or `~/…`. With `folder = "Projects/Specs"`, the file
  `Projects/Specs/a.docx` in SharePoint is `/foo/bar/a.docx` locally. Neither the site's name nor the folder's
  parents appear anywhere in the local path.
- **`name`:** what `--library`, `status`, the tray and the logs call the library. It defaults to the last part of
  `path`, with `-2`, `-3` and so on added if that's taken.
- **`sync.root` becomes optional.** It's only the default parent for `add` without `--to`. `~/SharePoint` stays the
  default.
- **The old form keeps working:** `local = "engineering"` means `name = "engineering"`, `path = <root>/engineering`.
  Existing state stays where it is (it's keyed by drive id).
  - Each library has exactly one of `local` and `path`.
- **Checks when the config loads:**
  - names are unique;
  - no library's path is inside another's, or the same as another's;
  - a path isn't `/`, the home folder itself, or inside the data folder.
  - Two libraries may share a SharePoint library if their `folder`s don't overlap.
- **`include` / `exclude`** are relative to `folder` when it's set.
- **Set-aside edits** (`not-uploaded/`, for files SharePoint refused):
  - Libraries with `path`: kept in `<data folder>/not-uploaded/<name>/`, so nothing appears next to a folder the user
    chose (e.g. on the Desktop).
  - Libraries with `local`: unchanged, in `<root>/not-uploaded/<local>/`.
  - Either way, every set-aside file is reported with its full path, and the tray's status window gets an **Open**
    button for the folder.

### 3.1 Engine: a folder as the library's root

SharePoint's delta works only from the drive's root, not from a folder (OneDrive personal is the exception), so:

- **Following the folder:** the folder is looked up once and **tracked by item id, not by path**. If someone renames
  or moves it (or a parent) in SharePoint, syncing carries on with nothing renamed locally. The config's `folder` is
  left as written (the tool doesn't rewrite the user's file); it's only used to find the folder the first time.
  - **Not detected:** a folder moved *into* another library's synced folder in SharePoint. Both libraries then sync
    the same items. The config check catches this only for the paths as written.
- **Changes:** the delta still lists the whole drive. Items outside the folder are dropped before reconciling, the
  same way `include` drops them now.
  - Paths are made relative to the folder, so everything else in the engine sees the folder as the root.
  - The first full listing lists the folder only (`children` from its id), not the whole drive.
- **The folder is gone** (deleted, or no longer shared with you): nothing local is deleted. Syncing stops for that
  library with "the SharePoint folder `Projects/Specs` is gone, or no longer shared with you", pointing to the
  recycle bin and to `remove` + `add` for a different folder.
  - It never re-finds the folder by path by itself: a new folder at the old path would otherwise be treated as a
    fresh first sync into the existing local copy, and upload files that were deleted in SharePoint.
  - Same rule as a missing local root: a vanished root never deletes the other side.
- **A local item moved out of the folder** is a delete in SharePoint (recycle bin), as it is for a library today. A
  remote item moved out of the folder is a delete locally.
  - **Moved in:** delta reports a moved folder but not its contents again, so a folder that's new to the library is
    listed (`children`, one request per folder in it).
- **State:** a folder has its own state DB (keyed by drive and folder id), so two folders of one SharePoint library
  never share state.
- **Live updates:** unchanged. A notification is per drive, and the delta then filters.
- **Cost:** one extra lookup per run, only when the folder's path is needed (`access`, `web` links).

## 4. Setup file and setup link

The organization-specific values, so nobody types a GUID.

**The file** (`Contoso.looper-sharepoint`):

```toml
[account]
tenant    = "11111111-2222-3333-4444-555555555555"
client_id = "66666666-7777-8888-9999-000000000000"
app_name  = "Contoso SharePoint Sync"

# Optional:
[[library]]
site    = "https://contoso.sharepoint.com/sites/Engineering"
library = "Documents"
path    = "~/workspace/sharepoint/engineering"
```

**The link:** the same `[account]` values as a link, for chat and email:

```
looper-sharepoint://setup?tenant=11111111-…&client_id=66666666-…&app_name=Contoso%20SharePoint%20Sync
```

- **Allowed:**
  - `[account]`: `tenant`, `client_id`, `app_name`, `publisher`, `allowed_tenants`.
  - `[[library]]`: in the file only. A link carries only `[account]`; libraries are added with `add` links (§5).
  - Anything else is refused by name. A setup file or link never changes how sync behaves, such as deletes or
    direction.
- **`looper-sharepoint setup export [--with-libraries] [-o FILE] [--link]`:** writes the file, or prints the link,
  from the current config. Neither holds secrets: the tenant id and app id identify the organization and the app,
  nothing more.
- **Applying** (`looper-sharepoint setup <file or link>`, opening the file, or clicking the link):
  1. It shows what it will set: the organization, the app name, and the libraries it adds with their folders.
  2. It asks: a window in the tray, or a prompt in Terminal (`--yes` to skip it there).
  3. It writes or merges the config.
  4. It offers to sign in.
- **A different organization already set up:** refused, unless `--replace`. It never silently moves someone to
  another organization.
- **Why it's safe to open:** it can only name an app registration. Microsoft's sign-in page then shows that app and
  its publisher, and `allowed_tenants` (and, later, admin policy files) still wins over it.

## 5. `add <SharePoint link>`

```
looper-sharepoint add <link> [--to <folder>] [--library <name>] [--name <name>] [--read-only] [--no-sync]
```

The link is whatever the user copied from the browser or from Teams' "Open in SharePoint":

| Link | Adds |
|---|---|
| A site: `https://contoso.sharepoint.com/sites/Engineering` | The site's main document library (`--library NAME` for another; the others are listed) |
| A library: `…/sites/Engineering/Shared%20Documents`, or its `Forms/AllItems.aspx` page | That library |
| A folder: `…/Forms/AllItems.aspx?id=%2Fsites%2FEngineering%2FShared%20Documents%2FSpecs&viewid=…`, or `…/Shared%20Documents/Specs` | That folder, directly into `--to` (`folder = "Specs"`) |
| A sharing link to a folder: `https://contoso.sharepoint.com/:f:/s/Engineering/E…` | The same, found through Graph's `/shares` |
| A link to a file | Refused: "that's a file; paste the link to its folder" |

- **`--to`:** the local folder, anywhere.
  - Without it, a terminal asks, suggesting `<sync.root>/<name>`. With `--yes` or no terminal, that suggestion is
    used. The tray always shows a folder picker with the suggestion filled in.
  - The suggested name comes from the folder's name for a folder link. Otherwise it's the site's name, lowercased and
    with spaces as `-`. A library that isn't the site's default one adds `-<library>`.
- **What it checks before writing:**
  - signed in (if not, it runs `login` first on a terminal, or says to);
  - the link resolves to a library or folder you can see;
  - the folder isn't inside, or around, another library's folder;
  - the folder is empty or doesn't exist. A non-empty one is refused, unless `--adopt`, which matches existing files
    by hash, as the first sync already does;
  - it isn't configured already. That's reported, not an error.
- **Writing the config:** the change is made with `toml_edit`, so comments and order are kept.
  - Without `[account]`, it says to run `setup` first (with the file or link from whoever sent the tool), or to pass
    `--tenant` and `--client-id`.
- **After writing:**
  - A running host reloads (§6) and starts the first sync.
  - With no host, `add` runs the first sync in the foreground (skip with `--no-sync`).
  - It then says how to keep it running: the tray with start at login, or `looper-sharepoint run`.
- **`looper-sharepoint://add?url=<SharePoint link>&to=<folder>`:** the same, from a link someone sends. It always
  opens the tray's Add window to confirm. `to` is only the suggestion in the folder picker.
  - A link never adds anything by itself.
  - It never picks a non-empty folder without the user choosing it.
- **`remove <name>`:** takes a library out of the config. It never deletes files, local or in SharePoint.
  - It forgets the library's sync state (the state DB isn't the user's data), so adding it again later, perhaps to
    another folder, starts fresh instead of finding a "missing" local folder. Not when a running host failed to
    reload: that host may still be syncing it.
- **The tray:** **Add a SharePoint Folder…** opens the Add window. It has a paste box and a folder picker, and runs
  the same checks with the same errors.

## 6. Config reload

- **A running host** (`run` or the tray) reloads the config when the CLI changes it.
  - The IPC socket gets a `reload` message, sent by `add`, `remove` and `setup`.
  - The tray's Open Config File also reloads when the file is saved, after a short debounce.
- **Reload:**
  - The new config is checked first. A bad file keeps the old one running and reports the error.
  - Otherwise the service stops at the next point between runs, and restarts with the new config.
  - Libraries that didn't change keep their state.
- **A different account** (tenant or app) needs a new sign-in. The host says so.

## 7. `looper-sharepoint://` links

- **macOS:**
  - The app's `Info.plist` declares the scheme (`CFBundleURLTypes`), and the `.looper-sharepoint` file type
    (`CFBundleDocumentTypes`, with an exported UTI).
  - macOS registers both when the app is in Applications and has been opened once, so no installer is needed.
  - The tray receives them through Tauri's deep-link plugin and the "opened" event.
- **Windows (M5):** the installer registers the scheme and the file type under `HKCU\Software\Classes`.
- **The CLI accepts the same links** (`setup <link>`, `add <looper-sharepoint://add?…>`), so they also work without
  the app.
- **Handled:** `setup` and `add` only. Anything else opens the status window with "this link isn't supported by this
  version".
- **Every link action is shown and confirmed** in a window first, because anyone can send a link. The window names
  the organization, the SharePoint location and the local folder.
- **If the app isn't installed**, clicking a link does nothing visible. That's why the quickstart puts installing
  first, and why the setup file stays: an attachment can always be opened later.

## 8. Start at login

- **The tray app** registers as a login item with `SMAppService.mainApp` (macOS 13+). It shows in System Settings →
  General → Login Items, where it can also be turned off.
  - **Menu:** **Start at Login**, with a check mark.
  - **On by default** after the first successful sync, with one line in the status window saying so and where to turn
    it off. Once the user turns it off, it's never turned on again for them.
  - **Packaged builds in Applications only.** Dev builds, the dmg and translocated copies can't register; the item is
    disabled with the reason.
- **The CLI:** `looper-sharepoint start-at-login on|off|status`.
  - With the app installed, it controls the tray's login item: one switch, not two.
  - With only the CLI, `on` installs a LaunchAgent (`~/Library/LaunchAgents/…plist`) that runs `looper-sharepoint
    run --no-tui`, and `off` removes it.
  - `status` says which is in effect, and it refuses to have both.
- **Windows (M5):** the same menu item, using the `Run` registry key.

## 9. Help that's enough on its own

Written for a reader with nothing else: a coworker in a terminal, or an AI assistant asked to "set up SharePoint
sync".

- **`looper-sharepoint` with no arguments, and `--help`:** start with the setup steps in order, then list the
  commands by task (set up, sync, check, access).

  ```
  Sync SharePoint document libraries and folders to local folders, both ways, and keep them in sync.

  Set up (once):
    1. looper-sharepoint setup <file or link>     your organization's setup file or looper-sharepoint:// link
                                                 (ask whoever sent you this tool)
    2. looper-sharepoint login                    sign in in the browser
    3. looper-sharepoint add <SharePoint link> --to <folder>
                                                 a site, library or folder link copied from the browser
    4. looper-sharepoint start-at-login on        keep syncing after restarts (or use the menu-bar app)
  ```

- **Each command's `--help`:** what it does and what it never does (e.g. `remove`: "never deletes files"), every
  flag, and two or three examples with `contoso.com` links.
- **Errors name the next command**, e.g. "not set up: run `looper-sharepoint setup <file or link>` (ask whoever
  sent you this tool)".
- **`--json`** stays available everywhere, for scripts and assistants.
- **Tests keep it honest:**
  - Every command line in `docs/QUICKSTART.md`, the README and the help examples is parsed with clap
    (`try_parse_from`). Renaming a flag fails the build.
  - The top-level help is checked for the setup steps.

## 10. Docs

- **`docs/QUICKSTART.md`**, one page (draft in §13):
  - for a coworker: the app, the setup link or file, a SharePoint link;
  - for the person setting it up for others: `setup export`, and what to send;
  - for terminal users.
  - Only `contoso.com` examples.
- **README:** the Quick start section shrinks to one example and "For more examples and setup details, see
  [docs/QUICKSTART.md](docs/QUICKSTART.md)". It also corrects "Not yet: installers": the dmg exists.
- **user-guide.md:**
  - §2 points to the quickstart, and gains `setup`, `add`, `remove` and `start-at-login`.
  - §3 gains `path`, `folder` and `name`, and marks `sync.root` and `local` as the older form.
- Docs change when the code ships, as usual: `docs/` describes what exists.

## 11. Testing

- **Unit:**
  - **Links:** every kind in §5 parses to the right site, library and folder, including encoded `id=` values,
    `Forms/…` pages, Teams links and trailing slashes. Also `looper-sharepoint://` links, and that unknown actions
    and keys are refused.
  - **Config:**
    - `path`/`local` exclusivity, default and unique names, overlap refused, the old form unchanged;
    - `toml_edit` keeps comments and order.
  - **Folder root:**
    - delta items outside the folder are dropped and paths are made relative;
    - the folder renamed or moved by id: no local changes, `folder` updated;
    - the folder gone: nothing deleted, the library stops;
    - items moved in and out of the folder, in both directions.
  - **Setup:** allowed keys only, merge, a different tenant refused, `--replace`.
  - **Reload:** a bad config keeps the old one running.
  - **Help and docs:** every command line parses (§9).
- **Live (QA):**
  - `add` with a site link, a library link, a folder from the browser's address bar, and a sharing link, each into an
    arbitrary folder.
  - A folder root:
    - rename and move the folder in SharePoint during a sync;
    - delete it: nothing local deleted;
    - two libraries on different folders of one SharePoint library.
  - A first sync through a running host after `reload`.
  - `remove` leaves files.
  - `setup export --link` → `setup <link>` in a fresh config and data folder → `login` → `add`.
- **Probe first:** whether `/shares/{encoded URL}` also resolves plain folder URLs, not only sharing links. If it
  does, every link kind goes through it, and URL parsing is only the fallback.
- **Manual (you):**
  - clicking a `looper-sharepoint://` link in Mail, Safari and Teams;
  - opening a `.looper-sharepoint` file from Finder;
  - the tray's Add window;
  - Start at Login across a restart, and off in System Settings.

## 12. Decisions

| # | Question | Proposed |
|---|---|---|
| 1 | Setup **file and** `looper-sharepoint://` **link**, both from `setup export` | Yes (revised: both). The link is the easy path; the file still works where links aren't clickable or the app isn't installed yet |
| 2 | Setup files and links may add libraries (file) or nothing but `[account]` (link), never sync settings | Yes |
| 3 | `add` runs the first sync, unless `--no-sync` | Yes |
| 4 | Start at login on by default after the first sync, in the tray | Yes (as agreed) |
| 5 | A folder link syncs that folder's contents directly into the chosen local folder; no SharePoint names in the local path | Yes (revised). Engine work in §3.1 |
| 6 | Any local folder per library (`path`); `sync.root` only the default parent; `local` still accepted | Yes (revised) |
| 7 | Set-aside edits for `path` libraries kept in the data folder, not next to the chosen folder | Yes. The alternative, `<path> (not uploaded)` beside it, would clutter places like the Desktop |
| 8 | `remove` never deletes files | Yes |
| 9 | `looper-sharepoint://add` links always confirm in a window, and `to` is only a suggestion | Yes |

## 13. Appendix: `docs/QUICKSTART.md` draft

> # Quick start
>
> ## Start syncing a SharePoint site or folder
>
> You need the app, your organization's setup link (or a `Contoso.looper-sharepoint` file) from whoever set this up,
> and access to the site in SharePoint.
>
> 1. **Install:** open `Looper-SharePoint-<version>.dmg`, drag Looper SharePoint to Applications, and open it once. A
>    cloud appears in the menu bar.
> 2. **Set up:** click the setup link (or open the file). Looper SharePoint shows your organization and asks to
>    continue.
> 3. **Sign in** with your work account when the browser opens.
> 4. **Add a site or folder:** in SharePoint, open the site, library or folder you want, and copy the address from
>    the browser. Choose **Add a SharePoint Folder…** from the menu, paste it, and choose any local folder, for
>    example `~/workspace/engineering`.
>
> That's it. Files download, your edits upload within seconds, and changes in SharePoint arrive within about 30
> seconds. Looper SharePoint starts at login, so this keeps working after a restart.
>
> ## In Terminal
>
> ```sh
> looper-sharepoint setup "looper-sharepoint://setup?tenant=…&client_id=…&app_name=…"   # or the .looper-sharepoint file
> looper-sharepoint login
> looper-sharepoint add "https://contoso.sharepoint.com/sites/Engineering" --to ~/workspace/engineering
> looper-sharepoint start-at-login on
> ```
>
> More examples: a folder link into its own folder, a sharing link, a second site, `--read-only`, `remove`, `status`.
>
> ## Setting this up for others
>
> After your admin has registered the app ([admin-setup.md](admin-setup.md)) and you've signed in once:
>
> ```sh
> looper-sharepoint setup export --link                            # a link to paste into chat or email
> looper-sharepoint setup export -o Contoso.looper-sharepoint      # or a file; --with-libraries adds your libraries
> ```
>
> Send the link or file, the dmg, and the link to the site. Neither holds passwords or secrets.
