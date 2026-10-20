# Spike Plan: Prove Low-Latency SharePoint Sync (config-driven)

See `../research.md` for the background. Project: **`looper-sharepoint`** (binary `looper-sharepoint`), MIT/Apache-2.0 dual licensed.

## What the spike must prove (go/no-go)

| # | Question | Pass criterion |
|---|---|---|
| 1 | Can a Rust public client authenticate to SharePoint with delegated auth, on Mac and Windows? | PKCE login + silent refresh work on both; token in OS keychain |
| 2 | Can we fully mirror a library described in a config file? | Initial sync of a seeded library (nested folders, ~1k files, a few >100 MB) matches byte-for-byte (quickXorHash) |
| 3 | **Remote → local latency** | p95 ≤ 60s with poll only; report p50/p95 with socket.io push (hoping for ≤ 10s) |
| 4 | On-demand sync ("git pull") | `looper-sharepoint sync` while the daemon runs picks up a remote change in ≤ ~3s + transfer time |
| 5 | Remote deletes / renames / moves propagate correctly | Tracked by item ID; folder rename does not re-download children; delete honors `on_remote_delete` |
| 6 | Local → remote (basic) | New/modified/deleted local files upload with `If-Match`; conflicting edit yields a conflict copy, not data loss |
| 7 | Resilience | Survives restart (state DB), `410 Gone` resync, 429 with `Retry-After`, socket disconnect → falls back to fast polling |
| 8 | Throttling budget | Measured RU use matches the model (~1.3 RU/min/library at 45s) |

**Out of scope for the spike:** placeholders / Files On-Demand (CfAPI, File Provider), GUI, installer, enterprise
policy enforcement beyond what the config file can express, multi-account, sovereign clouds.

## Architecture (spike scale)

Single Rust binary, Cargo workspace ready to split later:

```
looper-sharepoint/
  crates/
    looper-sharepoint-core/     # config, state DB, reconcile engine (pure, unit-testable)
    looper-sharepoint-graph/    # thin reqwest Graph client: sites, drives, delta, upload sessions, socketIo, retry/throttle
    looper-sharepoint-auth/     # oauth2 PKCE + device code, keyring token cache
    looper-sharepoint-cli/      # `looper-sharepoint` binary: login, sync, daemon, status, trigger
  tools/
    fixtures/        # create test library, seed files, mutate, tear down (Graph, Rust)
    latency-bench/   # remote mutation → local observation timer
```

Key design points:
- **State DB (SQLite)** per library: `item_id, parent_id, name, etag, quickxor, size, mtime, local_path, local_hash`
  plus the delta token. It's the "base" in a 3-way compare (remote delta vs base vs local scan).
- **Change triggers** → a single debounced `SyncRequest` channel per library. Sources: socket.io notification, poll timer,
  local FS watcher (`notify`), and an IPC `trigger` (Unix socket / Windows named pipe via the `interprocess` crate).
- Global throttle gate: any 429/503 pauses all requests for `Retry-After`.
- Write downloads to temp files + atomic rename; verify quickXorHash before rename.
- Start with plain-folder mode only. The engine API is designed so a placeholder backend can plug in later.

## Config file (spike scope)

`~/.config/looper-sharepoint/config.toml` (Windows: `%APPDATA%\looper-sharepoint\config.toml`):

```toml
[account]
tenant      = "contoso.onmicrosoft.com"   # or tenant GUID
client_id   = "00000000-0000-0000-0000-000000000000"  # bring-your-own app registration
auth_flow   = "pkce"                       # pkce | device_code
scopes      = ["Files.ReadWrite.All", "offline_access"]  # or Sites.Selected

[sync]
root                   = "~/SharePoint"
direction              = "two_way"   # download_only | two_way
push                   = true        # socket.io notifications
poll_interval          = "45s"       # safety net while push is healthy
poll_interval_degraded = "15s"       # when push is down
debounce               = "2s"
on_remote_delete       = "delete"    # delete | keep  (SharePoint recycle bin is the safety net)
max_delete_fraction    = 0.25        # pause + ask if a single pass deletes more than this
conflict               = "keep_both" # keep_both | remote_wins | local_wins

[[library]]
site    = "https://contoso.sharepoint.com/sites/Engineering"
library = "Documents"                # display name or drive id
local   = "engineering"              # relative to sync.root
include = ["specs/**", "*.md"]      # optional glob filter
exclude = ["**/~$*", "**/*.tmp"]
read_only = false

[[library]]
site    = "https://contoso.sharepoint.com/sites/Legal"
library = "Contracts"
local   = "legal"
read_only = true                     # never upload

[policy]                             # enterprise knobs; later overridable by an admin-managed file
allowed_tenants    = ["contoso.onmicrosoft.com"]
blocked_extensions = [".exe", ".dll"]
audit_log          = "~/.local/state/looper-sharepoint/audit.jsonl"
```

The spike implements everything above except `allowed_tenants` (validated only). The `[policy]` section is the
seed of the enterprise story: later, a GPO/MDM-delivered file at a system path overrides user config and is
merged with "admin wins" precedence.

## CLI

```
looper-sharepoint login                 # PKCE in browser (or --device-code)
looper-sharepoint libraries             # discover sites/libraries the user can see (helps write config)
looper-sharepoint sync [--library X]    # one-shot: delta + reconcile, then exit  (the "git pull")
looper-sharepoint daemon                # long-running: push + poll + FS watch
looper-sharepoint trigger [--library X] # ask running daemon to sync now via IPC
looper-sharepoint status                # last sync, pending ops, push health, RU estimate
```

## Milestones

**M0: QA tenant + app registration — DONE 2026-10-11** (procedure: `docs/admin-setup.md`, IDs: `.env`)
Target tenant: **"Seth Call Dev Tenant"** (`sethcallgmail.onmicrosoft.com`, tenant `d2586a15-…`; Seth is Global Admin,
`sethcall.com` already verified). Checked 2026-10-10: no M365 SKUs, `GET /sites/root` returns "Tenant does not have a SPO license".
1. In admin.microsoft.com (as Global Admin), start a **Microsoft 365 Business Basic trial** (or buy 1–2 seats) in this tenant.
   SharePoint provisions automatically at `https://sethcallgmail.sharepoint.com` (the name comes from the initial domain and can't be changed).
2. Create native work accounts, e.g. `spqa1@sethcall.com` and `spqa2@sethcall.com`, and assign the licenses. Use these instead of the
   gmail-backed `#EXT#` admin account (an MSA-backed account is a poor SharePoint test user). Don't use the federated
   `bi.sethcall.com` / `wsfed-local` domains, which route sign-in through an external IdP.
3. Register a **public client** app `sharepoint-sync-dev` (redirect `http://localhost`, allow public client flows), delegated
   `Files.ReadWrite.All`, `Sites.Read.All`, `offline_access`, then grant admin consent. (The `az` CLI can do this once licensing exists.)
4. Create a long-lived site `sharepoint-sync-qa`. Fixture tool creates a fresh library per run.
5. Note: this tenant also hosts unrelated identity work (Beyond Identity apps, AD Connect, federated domains). The spike only adds an
   app registration, two users, and one site; it touches no tenant-wide policies.

**M1: Auth + discovery + one-shot download (2–3 days)**
- `login`, `libraries`, config parsing, `sync` in download-only mode with full enumeration via delta, state DB, quickXorHash verify.
- Exit: criterion 1 (Mac), criterion 2.

**M2: Incremental + latency harness (2–3 days)**. Implemented 2026-10-11; see `specs/m2-design.md`. It grew to
include the embeddable engine library, the `run` TUI host and the Tauri tray host.
- Delta-token persistence, deletes/renames/moves by ID, 410 resync, throttle gate, `daemon` with polling, `trigger` IPC.
- `latency-bench`: a second identity (or the same app) mutates files via Graph at random intervals. The daemon logs the observation time
  per item. Run ≥200 mutations of mixed types; output a CSV + p50/p95/max.
- Exit: criteria 3 (poll), 4, 5, 7 (partial).

**M3: socket.io push (1–2 days, highest technical risk)**: implemented; see `specs/m3-design.md` §10 (push p95 32 s vs 46 s polling; Graph batches notifications ~30 s)
- Get the endpoint, connect with `rust_socketio` (websocket transport), notification → debounced delta, reconnect + fallback.
- Re-run the bench with push on/off. Also test a file saved from Office Online (autosave lag).
- If `rust_socketio` doesn't work: fall back to a minimal hand-rolled Engine.IO v4/socket.io client over `tokio-tungstenite` (small protocol).
- Exit: criterion 3 (push), 7.

**M4: Local → remote (2–3 days)**: implemented; see `specs/m4-design.md` §11
- FS watcher + local scan vs base, simple PUT / upload sessions, `If-Match`, conflict copies, `read_only`, `max_delete_fraction`; deletes go to the SharePoint recycle bin (never permanentDelete).
- Exit: criterion 6.

**M5: Windows validation (1–2 days)**
- Build + run the M1–M4 suite on `sethc@seth-windows` over SSH (cargo build, run daemon, run bench from the Mac).
  Plain-folder mode needs no low-level Windows code, so driving over SSH is sufficient. CfAPI work later
  would justify more hands-on Windows effort.
- Exit: all criteria on Windows; criterion 8 from logged RU estimates.

**Total: ~2–2.5 weeks.** Deliverable: a working `looper-sharepoint` binary, latency report (CSV + summary), and a go/no-go note.
Delivered for macOS: [docs/spike-report.md](../docs/spike-report.md) (criterion 8 measured on macOS; Windows still pending).

## Decisions (resolved 2026-10-10)

- License: **MIT/Apache-2.0 dual**.
- Name: **`looper-sharepoint`**.
- QA tenant: Seth Call Dev Tenant + Business Basic trial (pending Seth starting the trial; it needs a card and converts to paid after 30 days).
- jamkazam.com: later, read-only smoke tests only.

## After the spike (not planned yet)

Placeholders (CfAPI first, since Windows is the enterprise-heavy platform; then macOS File Provider via a Swift shell), admin policy file
via GPO/MDM, `Sites.Selected` onboarding flow, verified multi-tenant client ID, audit log export, delete-on-revoke.
