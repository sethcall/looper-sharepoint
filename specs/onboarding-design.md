# Design: onboarding (setup file, `add <link>`, start at login, quickstart)

Status: **draft for review** (2026-10-20). Decisions to confirm in §10.

## 1. Goal

Telling a coworker how to start syncing a SharePoint site should take one message:

> Install the app, open the setup file I sent, sign in, then paste the link to the site. It syncs into
> `~/workspace/sharepoint/<name>` and keeps itself up to date, including after restarts.

Today the same coworker has to:

- put a hand-written `config.toml` in `~/Library/Application Support`;
- get the tenant id and app id from someone;
- paste `[[library]]` blocks from `libraries`;
- restart the host after every change;
- remember to start the app again after every restart.

This milestone removes each of those steps. It also makes `looper-sharepoint --help` complete enough that a person
or an AI assistant can set it up from the help alone (§7).

## 2. The pieces

| Piece | What it replaces |
|---|---|
| **Setup file** (`<name>.looper-sharepoint`), made by `setup export`, applied by `setup <file>` or by opening it | Sending the tenant id and app id, and writing `[account]` by hand |
| **`add <SharePoint link>`** | `libraries`, then pasting `[[library]]` blocks |
| **Config reload** in a running host | Restarting the host after a config change |
| **Start at login** (tray), on by default after the first sync | Starting the app again after every restart |
| **`docs/QUICKSTART.md`**, linked from the README | Reading the whole user guide to start |
| **Help written as instructions** | Guessing the order of commands |

## 3. Setup file

The organization-specific values in one small file, so nobody types a GUID.

```toml
# Contoso.looper-sharepoint: open with Looper SharePoint, or run `looper-sharepoint setup Contoso.looper-sharepoint`
[account]
tenant    = "11111111-2222-3333-4444-555555555555"
client_id = "66666666-7777-8888-9999-000000000000"
app_name  = "Contoso SharePoint Sync"

# Optional:
[sync]
root = "~/workspace/sharepoint"

[[library]]
site    = "https://contoso.sharepoint.com/sites/Engineering"
library = "Documents"
local   = "engineering"
```

- **Allowed keys:**
  - `[account]`: `tenant`, `client_id`, `app_name`, `publisher`, `allowed_tenants`.
  - `[sync]`: `root` only. It's a suggestion: it's used only when the user has no root yet.
  - `[[library]]`: as in the config.
  - Anything else is refused by name. A setup file can't change how sync behaves, such as deletes or direction.
- **`looper-sharepoint setup export [--with-libraries] [-o FILE]`:** writes this file from the current config. It
  holds no secrets: the tenant id and app id identify the organization and the app, nothing more.
- **Applying it** (`looper-sharepoint setup <file>`, or opening the file in Finder, or dropping it on the tray's
  setup window):
  1. It shows what it will set: the organization (tenant), the app name, the folder, and the libraries it adds.
  2. It asks, unless `--yes`.
  3. It writes or merges the config.
  4. It offers to sign in.
- **An existing config with a different tenant or app:** refused, unless `--replace`. A setup file never silently
  moves someone to another organization.
- **Why it's safe to open:** a setup file can only name an app registration, and Microsoft's sign-in page then shows
  that app and its publisher. `allowed_tenants` (and, later, admin policy files) still wins over it.

## 4. `add <SharePoint link>`

```
looper-sharepoint add <link> [--to <folder>] [--library <name>] [--read-only] [--no-sync]
```

The link is whatever the user copied from the browser or from Teams' "Open in SharePoint":

| Link | Adds |
|---|---|
| A site: `https://contoso.sharepoint.com/sites/Engineering` | The site's document library. If it has several, it lists them and asks for `--library` |
| A library: `…/sites/Engineering/Shared%20Documents`, or its `Forms/AllItems.aspx` page | That library |
| A folder: `…/Forms/AllItems.aspx?id=%2Fsites%2FEngineering%2FShared%20Documents%2FSpecs&viewid=…`, or `…/Shared%20Documents/Specs` | That library, limited to the folder (`include = ["Specs/**"]`) |
| A sharing link to a folder: `https://contoso.sharepoint.com/:f:/s/Engineering/E…` | The same, found through Graph's `/shares` |
| A link to a file | Refused: "that's a file; paste the link to its folder" |

- **The local folder:**
  - Without `--to`, it's `<root>/<name>`. The name comes from the site name, lowercased and with spaces as `-`, e.g.
    `engineering`.
    - If the library isn't the site's default one, `-<library>` is added.
    - If the name is taken, `-2` is added.
  - With `--to <folder>`:
    - If there's no root yet, the folder's parent becomes `sync.root`.
    - Otherwise the folder must be inside the root, and `add` says so if it isn't.
  - A folder link keeps its path inside the library. `Specs` arrives as `<local>/Specs/…`. Syncing a folder's
    contents directly into `<local>` needs engine work, so it's out of scope (§10, decision 5).
- **What it checks before writing:**
  - signed in (if not, it runs `login` first on a terminal, or says to);
  - the site and library resolve;
  - the local folder is empty or doesn't exist (a non-empty one is refused, unless `--adopt`, which matches existing
    files by hash as the first sync already does);
  - the library isn't configured already (that's reported, not an error).
- **Writing the config:** the change is made with `toml_edit`, so comments and order are kept. Creating the config
  if there's none needs `[account]`. Without it, `add` says to run `setup <file>` first, or to pass `--tenant` and
  `--client-id`.
- **After writing:**
  - A running host reloads the config (§5) and starts the first sync.
  - With no host, `add` runs the first sync in the foreground (skip with `--no-sync`).
  - It then prints how to keep it running: the tray with start at login, or `looper-sharepoint run`.
- **`remove <local name>`:** takes a library out of the config. It never deletes files, local or remote, and says
  where the local copy and its state are.
- **The tray:** **Add a SharePoint Folder…** opens the status window with a paste box and a folder picker
  (defaulting as above). It does the same checks and shows the same errors.

## 5. Config reload

- **A running host** (`run` or the tray) reloads the config when the CLI changes it. The IPC socket gets a `reload`
  message, sent by `add`, `remove` and `setup`.
  - The tray's Open Config File also reloads when the file is saved, after a short debounce.
- **Reload:** the new config is parsed and checked first. A bad file keeps the old one running and reports the error.
  - Otherwise the service stops at the next point between runs, and restarts with the new config.
  - Libraries that didn't change keep their state; they're keyed by drive id, as now.
- **Not in a reload:** a different account (tenant or app) requires signing in again; the host says so.

## 6. Start at login

- **The tray app** registers itself as a login item with `SMAppService.mainApp` (macOS 13+). It shows in System
  Settings → General → Login Items, where the user can also turn it off.
  - **Menu:** a **Start at Login** item with a check mark.
  - **On by default:** it's turned on after the first successful sync, with one line in the status window saying so
    and where to turn it off. Never turned on again once the user turns it off; that choice is remembered.
  - **Packaged builds only.** A build that isn't an app bundle in Applications (dev builds, the dmg, a translocated
    copy) can't register; the item is disabled with the reason.
- **The CLI:** `looper-sharepoint start-at-login on|off|status`.
  - With the app installed, it controls the tray's login item, so there's one switch, not two.
  - With only the CLI (no app), `on` installs a LaunchAgent (`~/Library/LaunchAgents/…plist`) that runs
    `looper-sharepoint run --no-tui`, and `off` removes it.
  - `status` says which one is in effect, and refuses to have both.
- **Windows:** the same menu item, using the `Run` registry key (M5).

## 7. Help that's enough on its own

The help is written for a reader who has nothing else: a coworker in a terminal, or an AI assistant asked to "set up
SharePoint sync".

- **`looper-sharepoint` with no arguments, and `--help`:** start with the setup steps in order, then list the
  commands by task (set up, sync, check, access).

  ```
  Sync SharePoint document libraries to local folders, both ways, and keep them in sync.

  Set up (once):
    1. looper-sharepoint setup <file>.looper-sharepoint   your organization's setup file (ask whoever sent this tool)
    2. looper-sharepoint login                            sign in in the browser
    3. looper-sharepoint add <SharePoint link>            a site, library or folder link copied from the browser
    4. looper-sharepoint start-at-login on                keep syncing after restarts (or use the menu-bar app)
  ```

- **Each command's `--help`:** a sentence on what it does and what it never does (e.g. `remove`: "never deletes
  files"), every flag, and two or three examples with `contoso.com` links.
- **Errors name the next command:** every user-facing error ends with what to run, e.g. "not set up: run
  `looper-sharepoint setup <file>` (ask your admin for it)".
- **`--json`** stays available everywhere, for scripts and assistants.
- **Tests keep it honest:**
  - Every command line in `docs/QUICKSTART.md`, the README and the help examples is parsed with clap
    (`try_parse_from`). Renaming a flag fails the build.
  - The top-level help is checked for the setup steps.

## 8. Docs

- **`docs/QUICKSTART.md`**, one page (draft in §11):
  - for a coworker: the app, the setup file, a link;
  - for the person setting it up for others: `setup export`, and what to send;
  - for terminal users.
  - Only `contoso.com` examples.
- **README:** the Quick start section shrinks to one example and "For more examples and setup details, see
  [docs/QUICKSTART.md](docs/QUICKSTART.md)." It also corrects "Not yet: installers": the dmg exists.
- **user-guide.md §2:** points to the quickstart, and gains `setup`, `add`, `remove` and `start-at-login`.
- Docs change when the code ships, as usual: `docs/` describes what exists.

## 9. Testing

- **Unit:**
  - Every link kind in §4 parses to the right site, library and folder, including encoded `id=` values, `Forms/…`
    pages, Teams links and trailing slashes.
  - Default local names, collisions, and `--to` with and without a root.
  - Setup files: allowed keys only, merge, a different tenant refused, `--replace`.
  - `toml_edit` keeps comments and order.
  - Reload: a bad config keeps the old one running.
  - Help and docs command lines parse (§7).
- **Live (QA):**
  - `add` with a site link, a library link, a folder from the browser address bar, and a sharing link.
  - A first sync through a running host after `reload`.
  - `remove` leaves files.
  - `setup export` → `setup` in a fresh data dir and config → `login` → `add`.
- **Probe first:** whether `/shares/{encoded URL}` also resolves plain (non-sharing) folder URLs. If it does, every
  link kind can go through it, and URL parsing becomes only the fallback.
- **Manual (you):** opening a `.looper-sharepoint` file from Finder or Mail; the tray's Add dialog; Start at Login
  across a restart, and turning it off in System Settings.

## 10. Decisions

| # | Question | Proposed |
|---|---|---|
| 1 | Setup file format: a restricted TOML config (`.looper-sharepoint`), not a link | Yes. A custom-scheme link (`looper-sharepoint://setup?…`) is often not clickable in Outlook and Teams; a file attachment always works. A link can be added later |
| 2 | A setup file may add libraries, but never sync settings | Yes |
| 3 | `add` runs the first sync, unless `--no-sync` | Yes |
| 4 | Start at login on by default after the first sync, in the tray | Yes (as agreed) |
| 5 | A folder link keeps its path inside the library (`<local>/Specs/…`) rather than syncing the folder's contents directly into `<local>` | Yes for now. Direct needs engine work: a library "root" other than the drive root. Revisit if people find the extra level annoying |
| 6 | One folder for everything (`sync.root`); `--to` must be inside it once set | Yes. Per-library absolute paths can come later |
| 7 | `remove` never deletes files | Yes |

## 11. Appendix: `docs/QUICKSTART.md` draft

> # Quick start
>
> ## Start syncing a SharePoint site
>
> You need the app, your organization's setup file (`Contoso.looper-sharepoint`, from whoever set this up), and access
> to the site in SharePoint.
>
> 1. **Install:** open `Looper-SharePoint-<version>.dmg` and drag Looper SharePoint to Applications. Open it; a cloud
>    appears in the menu bar.
> 2. **Set up:** open the setup file. Looper SharePoint shows your organization and asks to continue.
> 3. **Sign in** with your work account when the browser opens.
> 4. **Add a site:** in SharePoint, open the site, library or folder you want, and copy the address from the browser.
>    Choose **Add a SharePoint Folder…** from the menu, paste it, and choose where it goes (for example
>    `~/workspace/sharepoint/engineering`).
>
> That's it. Files download, your edits upload within seconds, and changes in SharePoint arrive within about 30
> seconds. Looper SharePoint starts at login, so this keeps working after a restart.
>
> ## In Terminal
>
> ```sh
> looper-sharepoint setup ~/Downloads/Contoso.looper-sharepoint
> looper-sharepoint login
> looper-sharepoint add "https://contoso.sharepoint.com/sites/Engineering" --to ~/workspace/sharepoint/engineering
> looper-sharepoint start-at-login on
> ```
>
> More examples: a folder link, a sharing link, a second library, `--read-only`, `remove`, `status`.
>
> ## Setting this up for others
>
> After your admin has registered the app ([admin-setup.md](admin-setup.md)) and you've signed in once:
>
> ```sh
> looper-sharepoint setup export -o Contoso.looper-sharepoint      # add --with-libraries to include your libraries
> ```
>
> Send that file, the dmg, and the link to the site. The file holds no passwords or secrets.
