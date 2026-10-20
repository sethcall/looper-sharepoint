# Design: put `looper-sharepoint` on the PATH

Status: **built 2026-10-14** (§11 decisions). Code: `crates/looper-sharepoint-engine/src/cli_link.rs`, the CLI's
`install-cli`/`uninstall-cli`, and the tray's menu and dialog. Tested: unit tests and the CLI integration test (§9).
Still to do: the manual tests in §9 (the administrator prompt, a non-admin account, moving the app, the `.dmg` and
translocated cases). User docs: `docs/user-guide.md` §2.1. Builds on the packaged app ([docs/packaging.md](../docs/packaging.md)).
The app bundle contains the CLI at `Looper SharePoint.app/Contents/MacOS/looper-sharepoint`. Installing from the
`.dmg` is drag-and-drop, which can't run setup steps, so nothing reaches the PATH today.

## 1. Goal

After one click in the tray, or one command, typing `looper-sharepoint` in a new terminal runs the CLI from the
installed app:

- **Admins:** the CLI goes on the PATH for every user.
- **Standard users without admin rights:** the CLI goes on their own PATH.
- **The app moves or is updated:** the link keeps working, or the app repairs it.
- **Never touched without a click or command:** a file the app didn't create, and the user's shell configuration.

### Exit criteria

- On a fresh Mac, from the tray: Install Command Line Tool… → choose "all users" → admin password → a new Terminal
  window runs `looper-sharepoint status`.
- The same as a standard user with no admin password: "just me" → a new Terminal runs it. That includes the case where
  `~/.local/bin` wasn't on the PATH before, provided the user accepted the offer to add it.
- From the CLI: `"/Applications/Looper SharePoint.app/Contents/MacOS/looper-sharepoint" install-cli` does the same.
- Moving the app from `/Applications` to `~/Applications` and relaunching repairs a "just me" link without
  prompting.
- Remove Command Line Tool… / `uninstall-cli` removes only a link this feature created.
- Never: a prompt when nothing has to change, a replaced file the app didn't create, or a link pointing into a disk
  image or a translocated copy (§3).

## 2. Where the link goes

The CLI is installed as a **symlink**, not a copy, so updating the app updates the CLI.

| Scope | Link | Needs | On the PATH by default? |
|---|---|---|---|
| **All users** | `/usr/local/bin/looper-sharepoint` | Admin rights: `/usr/local/bin` is root-owned on current macOS, and may not exist on a fresh Apple Silicon Mac | Yes: `/etc/paths` lists it for every shell |
| **Just me** | `~/.local/bin/looper-sharepoint` | Nothing | Often not. §5 covers adding it |

- `~/.local/bin` is the XDG convention, and already used by pipx, uv and others.
- `/opt/homebrew/bin` is not used: it belongs to Homebrew.
- If `/usr/local/bin` happens to be writable by the user (some Intel Homebrew setups), "all users" needs no prompt.

## 3. Finding the CLI

This applies to both entry points.

- **Tray:** the CLI is next to the tray's own executable. That's `std::env::current_exe()`, canonicalized, then its
  parent joined with `looper-sharepoint`. This covers the app bundle (`Contents/MacOS/`) and a dev build
  (`target/release/`, when both binaries were built).
- **CLI:** the CLI is its own `current_exe()`, canonicalized. So running it through an existing link resolves to the
  real file inside the app, and re-installing is a no-op.
- **Checks before linking.** The candidate must:
  1. exist and be executable;
  2. answer `looper-sharepoint --version` with the same version as the tray, so a stale file left next to it is
     caught (tray only);
  3. **not be in a temporary location.** The app refuses, with an explanation, when the path is:
     - inside a mounted disk image (`/Volumes/…`). Tell the user to drag the app to Applications first.
     - inside an App Translocation path (`…/AppTranslocation/…`). macOS runs a quarantined app from a random
       read-only copy when it's opened straight from Downloads. Tell the user to move the app to Applications, or
       `~/Applications`, and reopen it.
- **Dev builds** (a path containing `/target/release/` or `/target/debug/`) are allowed. The confirmation says it's a
  development build.
- If no valid CLI is found, the tray's menu item is disabled, and the status window says why.

## 4. Installing

### 4.1 Tray

The tray menu shows **Install Command Line Tool…**, next to Open Config File, when no recorded link exists (§6). It
opens a small dialog in the status window:

> Install the `looper-sharepoint` command so you can use it in Terminal.
> ◉ For all users: `/usr/local/bin` (asks for an administrator password)
> ○ Just for me: `~/.local/bin`
> [Cancel] [Install]

- "All users" is preselected when the user is in the `admin` group (`id -Gn`). For a non-admin user it's hidden and
  "Just for me" is the only choice, so they're never asked for a password they don't have.
- **All users, needing admin:**
  - The app runs `mkdir -p /usr/local/bin` and `ln -sfn <cli> /usr/local/bin/looper-sharepoint` through
    `osascript -e 'do shell script "…" with administrator privileges'`. That shows the standard macOS password and
    Touch ID prompt.
  - Arguments are quoted, and nothing else runs as root.
  - Cancelling the prompt changes nothing and offers "Just for me" instead.
- **Just for me:** `mkdir -p ~/.local/bin`, then the link. No prompt. Then the PATH check (§5).
- **Result:** shows the link path and the command to try (`looper-sharepoint status`), and notes that already-open
  terminals need to be reopened.

### 4.2 CLI

```
looper-sharepoint install-cli [--user | --system] [--add-to-path]
looper-sharepoint uninstall-cli [--yes]
```

`uninstall-cli` asks before removing the PATH block it added (on a terminal); `--yes` removes it without asking.

- **No scope flag:** `--system` if `/usr/local/bin` is writable without elevation, else `--user`. The CLI never
  escalates without the flag being given.
- **`--system`:** if elevation is needed, runs exactly two commands under `sudo` (`/bin/mkdir -p /usr/local/bin`,
  `/bin/ln -sfn …`). They prompt in the terminal as usual.
  - The CLI itself never runs as root, so the record (§6) stays in the invoking user's data folder.
  - If `sudo` fails because the user isn't an admin, it says so and suggests `--user`.
- **`--add-to-path`:** with `--user`, performs §5's shell-file edit without asking. Without it, the CLI prints the
  line to add.
- **Already installed and pointing at this CLI:** prints where, and exits 0. That makes it idempotent, safe for
  scripts and MDM.

### 4.3 What already exists at the link path

| Found | Action |
|---|---|
| Nothing | Create the link |
| A symlink recorded as ours (§6), or any symlink whose target is a `looper-sharepoint` inside a `Looper SharePoint.app` | Replace it |
| Anything else: a regular file (e.g. `cargo install`), or a symlink to something else | **Stop.** Say what is there and leave it. With the CLI, `--force` replaces a symlink, never a regular file |

**Shadowing:** after installing, the app resolves `looper-sharepoint` on the user's login-shell PATH (§5). If a
different one comes first, for example `~/.cargo/bin/looper-sharepoint` from a source install, it warns: "your shell
will run <that path> instead".

## 5. The PATH check, for "Just for me"

- **The tray can't use its own PATH:** apps started by launchd get a minimal one. So the app asks the user's shell:
  `$SHELL -l -i -c 'printf "<<%s>>" "$PATH"'`, with a 3 s timeout. It then takes the text between the markers,
  because interactive shell startup files may print other output.
- **If `~/.local/bin` is already on that PATH:** done.
- **If not:** offer **Add to my shell startup file** (tray: a button; CLI: `--add-to-path`, otherwise just print the
  line).

| `$SHELL` | File | Line added |
|---|---|---|
| zsh | `~/.zprofile` | `export PATH="$HOME/.local/bin:$PATH"` |
| bash | `~/.bash_profile` | same |
| fish | none: runs `fish -c 'fish_add_path -U ~/.local/bin'` | |
| other | none: show the line to add by hand | |

- **zsh and bash:** use the login-shell files, because macOS Terminal opens login shells. The line is appended
  between marker comments (`# >>> looper-sharepoint >>>` … `# <<< … <<<`).
- **Repeating it is a no-op:** if the markers or the exact line are already there, nothing is added.
- **Nothing else in the file is touched.** If the check failed (timeout or an unknown shell), the app shows the
  line and changes nothing.

## 6. Ownership record, repair and removal

- **The record:** `cli-link.json` in the data folder, holding `{ link, target, scope, installed_at, path_edit }`.
  `path_edit` names the shell file edited, if any. The record is how the app knows a link is its own.
- **Repair at tray launch:**
  - **Our link, target gone** (the app was moved, renamed, or a second copy was deleted):
    - "Just me": re-point it at the current CLI silently.
    - "All users": re-point it silently if no elevation is needed. Otherwise the menu shows **Repair Command Line
      Tool…**. The app never prompts for a password at launch.
  - **Our link, target is another valid copy of the app:** leave it. One copy owns the command.
  - **The link has been replaced by something else:** forget the record and touch nothing.
- **Removal:** Remove Command Line Tool… (the menu shows it when a recorded link exists) / `uninstall-cli`.
  - Deletes the link only if it's still ours, using admin rights for "all users" when needed.
  - Offers to remove the marked PATH block, if we added one.
  - Then clears the record.
- **Deleting the app** by dragging it to the Trash leaves a dangling link. That's harmless: the shell reports
  "no such file". The user guide says to choose Remove Command Line Tool… first.

## 7. Status

- **Tray status window:** "Command line tool: `/usr/local/bin/looper-sharepoint`", or "not installed", with the
  shadowing warning if any.
- **CLI:** `looper-sharepoint status` and `whoami` don't change. `install-cli` with nothing to do reports the current
  state.

## 8. Docs

- **`docs/user-guide.md`, Setting up:**
  - Install from the `.dmg`. Standard users drag the app to `~/Applications` instead of Applications.
  - Then Install Command Line Tool….
  - How to uninstall.
- **`docs/packaging.md`:** note where the CLI sits in the bundle.

## 9. Testing

- **Unit tests,** on pure functions over a temp directory:
  - Choosing the scope and the action for each existing-file case (§4.3).
  - Detecting temporary locations: `/Volumes/…`, `…/AppTranslocation/…`, dev paths.
  - The PATH-marker parsing.
  - Adding the shell-file block: idempotent, and appended only.
  - Repair decisions (§6).
- **CLI integration test**, with `HOME` and the data folder in a temp directory: `install-cli --user --add-to-path`,
  run it again (no-op), `uninstall-cli`, and check the shell file and the link each time.
- **Manual, on this Mac with the signed build:**
  - "All users" with the password prompt, then cancelling the prompt.
  - "Just me".
  - Moving the app to `~/Applications` and relaunching (repair).
  - Opening it from the mounted `.dmg` (refused).
  - Opening it from Downloads (translocated, refused).
  - A non-admin test account on this Mac.

## 10. Not in this design

- Windows (adding to the user `PATH` in the registry). That's with M5.
- Installing without being asked, e.g. on first launch.
- Shell completions and man pages. They could reuse §2–§6 later.
- Homebrew distribution (`brew install --cask`). It would put the CLI on the PATH through Homebrew instead, and could
  coexist via the shadowing warning.

## 11. Decisions (approved 2026-10-11)

| # | Question | Decision |
|---|---|---|
| 1 | Default scope for admins in the tray | "All users" preselected; "Just for me" one click away |
| 2 | Offer to edit shell startup files at all | Yes, only on an explicit click or `--add-to-path`, marked and removable |
| 3 | Repair silently at launch | Yes for "just me" and for a writable `/usr/local/bin`; never prompt for a password at launch |
| 4 | Command names | `install-cli` / `uninstall-cli` |
