# M1 Design: Auth, Discovery, One-Shot Download

Status: **implemented, pending sign-off** (2026-10-11). See "Implementation notes" at the end for how the build differs from this spec. Part of `specs/spike-plan.md`. Moves to `docs/` as each piece ships.

## Scope

In:
- the Cargo workspace
- `config.toml` parsing
- `login` / `logout` / `whoami`
- `libraries` (discovery)
- `sync` in **download-only** mode: initial enumeration via delta, plus re-runs that apply incremental adds and modifies
- local state DB
- quickXorHash
- the throttle/retry gate
- the test fixtures tool

Out (later milestones):
- daemon, push, polling (M2/M3)
- uploads of user files (M4); the fixtures tool uploads, so the Graph upload code lands now
- FS watching
- placeholders
- policy files (see `specs/policy.md`); M1 only *reads* the user-config keys that policy will later be able to lock

Exit criteria (from the spike plan):
1. Login and silent refresh work on macOS.
2. A seeded library (~1k files, nested folders, a few files >100 MB) syncs and every file matches its remote quickXorHash.
3. Re-running `sync` downloads nothing.
4. Remote edits followed by `sync` download only the changed files.

## Workspace

```
crates/
  looper-sharepoint-core/    # config, state DB, path mapping, reconcile (pure logic, no network)
  looper-sharepoint-graph/   # our own thin Graph client over reqwest (no onedrive-api / graph-rs-sdk)
  looper-sharepoint-auth/    # PKCE + device code (oauth2 crate), token store
  looper-sharepoint/         # CLI binary (clap)
tools/fixtures/            # binary: seed/mutate/clean test data in the QA site
```

Main dependencies:
- tokio
- reqwest (rustls)
- serde + toml
- rusqlite (bundled)
- oauth2
- keyring
- clap
- tracing
- unicode-normalization
- globset
- dirs
- thiserror / anyhow (anyhow in the binary only)

License files: `LICENSE-MIT`, `LICENSE-APACHE`.

## 1. Local state DB

- **Engine:** SQLite, local only.
- **One DB per configured library:** `<data_dir>/looper-sharepoint/state/<drive_id_sha256_16>.sqlite`. On macOS the data dir is
  `~/Library/Application Support`; on Windows it's `%LOCALAPPDATA%`.
- **Never inside the sync root**, so it's never synced, never seen by watchers, and survives the user wiping the folder.
- **Modes:** WAL mode, one writer.

```sql
CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
-- schema_version, tenant_id, site_id, drive_id, local_root, delta_link, last_full_sync_at

CREATE TABLE items (
  item_id        TEXT PRIMARY KEY,      -- Graph driveItem id; identity survives rename/move
  parent_id      TEXT,                  -- null for the library root
  remote_name    TEXT NOT NULL,         -- exact name in SharePoint
  is_dir         INTEGER NOT NULL,
  -- remote state, as of the last delta we applied
  r_etag         TEXT,
  r_quickxor     TEXT,
  r_size         INTEGER,
  r_mtime        TEXT,                  -- fileSystemInfo.lastModifiedDateTime
  -- local state, as of the last time we wrote or verified the file ("base")
  local_path     TEXT,                  -- relative to library root, as written on disk
  local_key      TEXT,                  -- NFC + case-folded local_path, for collision checks
  l_size         INTEGER,
  l_mtime_ns     INTEGER,
  l_quickxor     TEXT,
  state          TEXT NOT NULL,         -- synced | pending | skipped | conflict | error
  reason         TEXT,                  -- why skipped/conflict/error
  updated_at     TEXT NOT NULL
);
CREATE INDEX items_parent ON items(parent_id);
CREATE UNIQUE INDEX items_local_key ON items(local_key) WHERE local_key IS NOT NULL;
```

- **Paths:** derived from the `parent_id` chain plus mapped names and cached in `local_path`. A folder rename means rewriting
  the subtree's cached paths in a transaction plus one `rename()` on disk. Children are never re-downloaded.
- **Base state:** a local file is unchanged if its `(size, mtime_ns)` matches the base. On mismatch we rehash before
  deciding. Cheap scans now, correct 3-way compare in M4.
- **Migrations:** numbered SQL files keyed off `schema_version`, applied at open. The spike can wipe and resync if needed.

## 2. Path mapping (remote name ↔ local name)

SharePoint already rejects most names that Windows forbids:
- the characters `" * : < > ? / \ |`
- leading and trailing spaces
- `CON`, `PRN`, `AUX`, `NUL`, `COM0-9`, `LPT0-9`
- `.lock`, `_vti_`, `desktop.ini`

SharePoint is also case-insensitive, like default APFS and NTFS. So collisions are rare, but mapping is still needed for:
- **Unicode normalization.** SharePoint names may be NFC or NFD. Compare everything as NFC. Write NFC on Windows; on macOS write NFC
  (APFS preserves it and treats both forms as equal).
- **Characters SharePoint allows but the local OS doesn't.** Mainly trailing dots/spaces on Windows and anything a future tenant setting permits.
- **Path length.** SharePoint allows 400 characters; Windows MAX_PATH is 260. Use `\\?\` extended paths on Windows
  everywhere (no mapping needed, but tools like Explorer may struggle; warn above 260).
- **Collisions after mapping.** Two remote names that map to the same `local_key`.

**Default (`naming.mode = "escape"`):**
- An unrepresentable character becomes its Unicode full-width lookalike, the same approach as rclone.
  For example, `:` → `：` (U+FF1A) and a trailing `.` → `．` (U+FF0E).
- This is reversible, so M4 uploads map back exactly.
- If a remote name already contains a lookalike, it's left alone and recorded in the DB so the reverse map stays unambiguous.
- Collision: the second item gets ` (2)` before the extension, is marked `skipped` with reason `name_collision`, and is reported.

**Configurable:**
```toml
[naming]
mode = "escape"            # escape | skip   (skip: don't sync unrepresentable names; report them)
[naming.char_map]          # override/extend the default lookalike table (remote char -> local char)
":" = "꞉"
"#" = "＃"
```
The map must be a bijection. Validation fails at config load if two keys map to the same char, or if any value
is itself illegal locally. `naming.*` is a policy-lockable key (see `specs/policy.md`).

## 3. Download-only semantics in M1

Safety model: **the server is the safety net, not the client.** SharePoint already has a two-stage recycle bin (93 days)
and per-library versioning (on by default, 500 versions). So the client does **not** keep a local quarantine/trash, and strict
admins get what they want: deleted data is gone from devices. What the client must guarantee is that it never loses
data that exists *only* locally, and never pushes mass destruction to the server (M4).

| Situation | Default | Config key (lockable by policy) |
|---|---|---|
| **Remote delete:** an item we previously synced was deleted in SharePoint, and the local copy matches base | Delete the local copy (like OneDrive). Recovery is via the SharePoint recycle bin | `sync.on_remote_delete = "delete" \| "keep"` |
| **Remote delete**, but the local copy was modified since base (changes that exist only locally) | Keep it, mark it `conflict`, report it. In M4 two-way mode, re-upload it instead | n/a: never silently destroyed |
| **Local orphan:** a file exists locally but was never seen remotely | Keep it, report it once per run | `sync.local_orphans = "keep" \| "delete"` (strict mirror is a hardened, policy-intended setting) |
| **Local file modified** (doesn't match base) and remote also changed | Don't overwrite. Mark `conflict`, report it | (M4 adds conflict copies) |
| **Local file modified**, remote unchanged | Leave it. Download-only mode doesn't upload | n/a |
| **Pre-existing local file at the target path** on first sync | If its quickXorHash matches remote, adopt it into the DB (no download). Otherwise `conflict` | n/a |

`sync.max_delete_fraction` (default 0.25): if one run would delete more than this share of tracked local files, it stops
before deleting anything and asks the user to re-run with `--allow-mass-delete`. This catches a bad delta or a 410-resync bug in M1.
In M4 the same guard protects the server from a local `rm -rf`. Every local delete is written to the run log
(with item ID and remote URL, so the user can find the file in the SharePoint recycle bin).

Server-side invariants (enforced from M4, stated here so the Graph layer is built for them):
- Deletes use `DELETE /items/{id}`, which goes to the recycle bin. **Never `permanentDelete`.**
- Content changes are uploaded **in place** on the existing item ID (keeps version history). Never delete and re-create.

**Download mechanics:**
1. Stream `GET /drives/{d}/items/{i}/content` (follow the 302; don't send the auth header to the redirect target) to
   `<root>/.looper-sharepoint-tmp/<item_id>.part`. On Windows, mark that directory hidden.
2. Compute quickXorHash while streaming.
3. On mismatch, retry once, then mark `error`.
4. Set the mtime from `fileSystemInfo.lastModifiedDateTime`, then atomically rename into place and update the DB row in the same step.
5. Concurrency: up to `sync.max_parallel_downloads` (default 4).

## 4. Graph client, errors, throttling

All requests go through one `GraphClient`, which:
- sends `User-Agent: ISV|looper-sharepoint|looper-sharepoint/<version>`
- adds the bearer token and, on a 401, refreshes once and retries
- **throttle gate:** a shared `pause_until: Instant`. Any 429/503 with `Retry-After` sets it, and every request awaits it before sending.
  Without `Retry-After`, it uses exponential backoff with jitter (1s → 60s cap, max 6 attempts).
- separates error kinds:
  - `Retryable` (429, 503, 504, network)
  - `Auth` (401 after refresh, `invalid_grant`): stop and tell the user to log in again
  - `ResyncRequired` (410): drop the delta link and re-enumerate
  - `NotFound`
  - `Fatal`

Endpoints in M1:
- `/me`
- `/sites/{host}:/{path}`
- `/sites/{id}/drives`
- `/me/followedSites`
- `/sites?search=`
- `/drives/{id}/root/delta` (with paging and `@odata.deltaLink`)
- `/items/{id}/content`
- simple PUT and upload sessions (fixtures only)

Delta is always run at the **library root**, because SharePoint doesn't support folder-scoped delta. Folder scoping is
a client-side filter (`include` / `exclude` globs).

## 5. Auth

- **Default flow:** `login` uses auth code + PKCE with the redirect `http://localhost:<random port>`; Entra accepts any port for loopback.
  It opens the browser and waits up to 5 minutes.
- **Fallback:** `--device-code`.
- **Scopes:** from config. The default is `Files.ReadWrite.All Sites.Read.All offline_access User.Read openid profile`.
- **Token storage:**
  - The refresh token goes in the OS keychain under service `looper-sharepoint`, account `{tenant_id}/{client_id}/{upn}`. Access tokens are kept in memory only.
  - **Known risk:** Windows Credential Manager caps a credential at 2,560 bytes, and Entra refresh tokens can be close to or over that. The Windows fallback
    is a DPAPI-encrypted (CurrentUser) file in `%LOCALAPPDATA%`. Decide this in M5 after measuring real tokens.
- **`whoami`:** prints the user, tenant, and token expiry.
- **`logout`:** deletes the stored token.

## 6. Config (M1 subset)

These are the keys from the spike plan's `config.toml` that M1 reads:
- `account.*`
- `sync.root`
- `sync.direction` (only `download_only` is accepted)
- `sync.on_remote_delete`
- `sync.local_orphans`
- `sync.max_delete_fraction`
- `sync.max_parallel_downloads`
- `[naming]`
- `[[library]]`, with keys `site`, `library`, `local`, `include`, `exclude`

Unknown keys are an error. That prevents silent typos in a tool that may delete files.

- **`--config <path>`** overrides the default location.
- **`looper-sharepoint config check`** validates the file and resolves each site and library to its IDs.

## 7. CLI (M1)

```
looper-sharepoint login [--device-code]
looper-sharepoint logout
looper-sharepoint whoami
looper-sharepoint libraries [--site <url>]   # lists sites/libraries + drive ids, prints ready-to-paste [[library]] blocks
looper-sharepoint config check
looper-sharepoint sync [--library <local>] [--dry-run] [--allow-mass-delete]
looper-sharepoint status                     # per library: last sync, counts by state, conflicts, recent local deletes
```

Output is human-readable by default; `--json` gives machine output on every command. The exit code is nonzero if any item ended in `error`
or `conflict`.

## 8. Testing

- **Unit tests** (`cargo test`, offline, the default):
  - quickXorHash against known vectors (rclone's test vectors, plus the hash Graph returned for our `smoke/hello.txt`)
  - path mapping round-trips and collisions
  - config validation
  - the reconcile decision table in §3, driven by recorded or fake delta pages
- **Live tests** (`cargo test --features live -- --test-threads=1`):
  - Read tenant, app, and drive IDs from `.env`, and sign in as `spqa1`.
  - The ROPC password grant is used *only* in tests, because the test account has no MFA.
  - Each run works in its own folder `runs/<timestamp>-<rand>/` in the QA library, with `include = ["runs/<id>/**"]`, and deletes the folder at the end.
  - We don't use a library per run, because creating libraries needs `Sites.Manage.All`. Revisit that in M2 if root-level delta noise from other runs gets in the way.
- **`tools/fixtures`:**
  - `seed --files 1000 --large 3 --large-size 150MB`
  - `mutate --edits N --deletes N --renames N`
  - `clean`
  - Uses the same graph crate.

## Decisions

- `looper-sharepoint restore` is post-spike (see `specs/policy.md`).

## Implementation notes (2026-10-11)

Differences between the build and the spec above:
- **OAuth:** hand-rolled on reqwest (PKCE, device code, refresh, about 300 lines) instead of the `oauth2` crate, to avoid a second
  reqwest version and adapter code. `keyring` is pinned to 3.x (4.x is a new architecture).
- **Mass-delete guard:** applies only when at least **10** files would be deleted, so tiny libraries aren't blocked by percentages.
- **Delta link:** saved only if the run had no errors. A failed run replays the same delta next time, which is safe because
  staging is idempotent.
- **Local evaluation:** every tracked file is re-checked against disk each run (stat, and a hash only on mismatch). So a run also
  restores locally deleted files and retries earlier errors. Revisit for very large libraries once the M2 file watcher exists.
- **Transactions:** a run is one SQLite transaction (rolled back for `--dry-run`). If a run crashes, files already downloaded are
  *adopted* on the next run because their hash matches, so nothing downloads twice.
- **Test hooks:**
  - `LOOPER_SHAREPOINT_DATA_DIR` overrides the state directory.
  - `LOOPER_SHAREPOINT_TEST_USERNAME` / `LOOPER_SHAREPOINT_TEST_PASSWORD` switch to the password grant (MFA-less test accounts only).
- **Tooling:** `tools/fixtures` also has `put`, `list` and `verify` (hash-compare a local tree against SharePoint).

