# M2 Design: the embeddable sync service, CLI host, and tray host

Status: **implemented, pending sign-off** (see §11 for results and where the build differs from this plan). Builds on `specs/m1-design.md` (the one-shot download-only engine) and the principles in
`docs/sharepoint/throttling.md`.

## 1. Goal

Turn the one-shot `sync` into a long-running service that keeps libraries current on its own, and ship it three ways:

1. **A library**, `looper-sharepoint-engine`, that any desktop app can embed. It does all the sync work: scheduling,
   throttling discipline, state, history and status. It does **no** windowing, daemonizing, signal handling, terminal
   output or process management. Those belong to the host app.
2. **A CLI host**: `looper-sharepoint run` runs the service in the foreground with a TUI.
3. **A tray host**: `looper-sharepoint-tray`, a Tauri v2 menu-bar/notification-area app whose windows open from the tray.

The two hosts must use only the library's public API. If a host needs something the API lacks, the API grows; the
host does not reach into internals. The tray is the proof that a third-party app could do the same.

The latency harness (spike-plan criteria 3 and 4) also lands in M2.

Exit criteria:

- Criterion 3 (poll): remote→local p95 ≤ 60s over ≥200 mutations.
- Criterion 4: `looper-sharepoint sync` against a running host picks up a change in ≤ ~3s plus transfer time.
- Criterion 7 (partial): survives restart, 410, 429, and expired sign-in. "Expired sign-in" means a status of
  "sign-in required", not a crash loop.
- Both hosts run on macOS. Windows is checked in M5.

## 2. Crates

```
crates/
  looper-sharepoint-core     (M1) config, state DB, naming, filters, reconcile rules, quickXorHash
  looper-sharepoint-graph    (M1) Graph client: + request pacer, + observable throttle state
  looper-sharepoint-auth     (M1) sign-in flows, keychain store
  looper-sharepoint-engine   NEW  engine (moved from the CLI) + Service: scheduler, status, history, events
  looper-sharepoint-ipc      NEW  optional local control socket: hosts serve it, `looper-sharepoint` CLI talks to it
  looper-sharepoint          CLI binary: one-shot commands + `run` (TUI host) + `trigger`
apps/
  tray/                    `looper-sharepoint-tray` (Tauri v2), a separate binary so the CLI never links a webview
tools/
  fixtures/                (M1)
  latency-bench/           NEW
```

IPC is a separate crate, not part of the engine, so an embedding app that doesn't want a control socket never opens
one. Both shipped hosts serve it, which lets `looper-sharepoint status | trigger | sync` drive whichever host is running.

## 3. Library API (`looper-sharepoint-engine`)

```rust
// One-shot (no scheduling), as in M1:
pub async fn sync_library(graph, cfg, lib, &RunOptions, progress: &dyn Fn(Progress)) -> Result<Report>;
pub async fn resolve(graph, lib) -> Result<Resolved>;

// Long-running:
let svc = Service::start(ServiceConfig { config, tokens, data_dir })?;   // inside a tokio runtime
svc.status() -> Status                       // cheap snapshot for rendering
svc.subscribe() -> broadcast::Receiver<Event>
svc.trigger(Some("eng"), Trigger::Manual)    // fire and forget; coalesces with a run in flight
svc.sync_now(None, RunOptions) -> Vec<RunRecord>   // waits; what `looper-sharepoint sync` uses against a host
svc.pause() / svc.resume()
svc.shutdown().await                         // finishes or abandons the in-flight run cleanly
```

- `Status`, `Event`, `RunRecord` and `Progress` are plain serde types, so IPC and the Tauri webview reuse them as JSON.
- **Status**:
  - Service level: account; throttle state (`throttled_until`); pause state; request counters.
  - Per library: `state` (Idle | Syncing{progress} | Waiting{until} | Error | SignInRequired | Paused);
    `last_success_at`; `last_failure` {at, error}; `next_run_at`; the latest run's counts.
- **Events**: `RunStarted`, `Progress`, `RunFinished(RunRecord)`, `Throttled{until}`, `StatusChanged`.
  - The broadcast channel is lossy for slow subscribers; they re-read `status()`.
- **History**: every run is recorded in a `runs` table in the library's state DB, so it survives restarts. Each
  record holds start, finish, trigger, outcome (ok | problems | failed | refused), summary counts, and the error.
  The last 200 runs per library are kept.
- **Single host per data dir**: `Service::start` takes an exclusive OS file lock on `<data_dir>/host.lock`, using
  std's `File::try_lock`. A second host fails fast with "already running (pid N)". One-shot `sync_library` uses the
  same lock. The CLI's `sync` therefore delegates to the running host instead of colliding with it.
- **Logging**: the library emits `tracing` events only. The host installs the subscriber.

## 4. Scheduling (implements throttling principles 5, 7, 9)

There is one task per library. They share one `GraphClient`, and therefore one throttle gate and one pacer.

| Trigger | Behaviour |
|---|---|
| Startup | Run once |
| Poll timer | Every `sync.poll_interval`, ±10% jitter so devices never poll in lockstep |
| Manual (`trigger`, TUI key, tray "Sync now") | Run now. If a run is in flight, queue **one** follow-up |
| Push (M3) | Same as manual, debounced |

- **Failure backoff**: after a failed run, the next attempt is at min(interval × 2^failures, 30 min). A success
  resets it. A run that is "refused" by the mass-delete guard isn't retried until a manual run with an override,
  or a config change.
- **Throttled**: the gate already pauses every request. In addition, the service reports `Waiting{until}` and pushes
  the next poll beyond the throttle window.
- **Sign-in required** (`invalid_grant`): polling stops, the state becomes `SignInRequired`, and polling resumes
  after a successful login. The tray offers "Sign in…".
- **Pacer**: a client-side limit of `sync.max_requests_per_second` (default **8**) per process, in the Graph client.
  It applies to every request, downloads included.

New config keys (`[sync]`):

| Key | Default | Notes |
|---|---|---|
| `poll_interval` | `"45s"` | Lower bound `"15s"`. Admin floor via policy later (`specs/policy.md`) |
| `max_requests_per_second` | `8` | 1–10. Above 10 the per-user limit is guaranteed to bite |

## 5. IPC (`looper-sharepoint-ipc`)

- **Transport**: a local socket via the `interprocess` crate: a Unix domain socket on macOS and Linux, a named pipe on
  Windows.
  - The name is derived from the data dir, so isolated test setups don't collide.
  - Unix: the socket lives in the data dir and is restricted to `0600`.
- **Protocol**: newline-delimited JSON. A request is `{"cmd": "status" | "trigger" | "sync" | "subscribe" | "pause" | "resume"}`.
  - `subscribe` keeps the connection open and streams `Event`s.
  - Any other request gets one response, then the server closes the connection.
- **Security**: local user only (file permissions / the pipe's default ACL). No auth beyond that in M2. The socket
  can only trigger and observe syncs that the config already permits.

## 6. CLI host

| Command | Change |
|---|---|
| `run [--no-tui]` | **New.** Foreground service. TUI when stdout is a terminal, plain log lines otherwise (`--no-tui`). Ctrl-C / `q` shuts down cleanly |
| `trigger [--library X]` | **New.** Asks the running host to sync; returns immediately |
| `sync` | If a host is running, delegates via IPC and waits (the "git pull"). Otherwise one-shot as in M1 |
| `status` | Live from the host when running (state, next run, throttle). Otherwise offline from the DBs. Both include recent history |

TUI (ratatui + crossterm):

```
 looper-sharepoint ─ spqa1@sethcall.com ─ up 2h14m ─ requests 1,204 (throttled 0) ───────────────
  LIBRARY        STATE                LAST SYNC     LAST FAILURE                      NEXT
 ▶ qa             idle                 12s ago       3h ago: 503 Service Unavailable   in 31s
   legal          syncing 340/1003     2m ago        never                             –
 ── recent runs ────────────────────────────────────────────────────────────────────────
  14:05:12  qa     poll    ok        2 downloaded, 1 moved              0.8s
  14:04:27  qa     poll    ok        no changes                         0.3s
  14:03:01  legal  manual  failed    sign-in expired; run `looper-sharepoint login`
 ── s sync selected · a sync all · p pause/resume · ↑↓ select · q quit ──────────────────
```

While the TUI is active, logs go to `<data_dir>/logs/looper-sharepoint.log` so they don't corrupt the screen.
Relative times refresh every second.

## 7. Tray host (`apps/tray`, Tauri v2)

- No window at startup and no Dock icon on macOS (accessory activation policy). There is one tray icon whose image
  shows the overall state: ok, syncing, attention (error or conflict), or offline/throttled.
- Menu, rebuilt on status changes:
  - "Up to date · synced 2m ago", or the current problem (disabled line)
  - One line per library: name, state and relative time; clicking it opens the local folder
  - Sync now
  - Pause / Resume
  - Status… (opens the status window)
  - Sign in… (only when needed)
  - Open config file
  - Quit
- **Status window**: plain HTML, CSS and JS (no npm toolchain), receiving live updates through Tauri events. It shows
  the same information as the TUI: libraries, last sync and last failure as relative times with the error, the
  recent-runs list, and the throttle state. Closing it hides it; the app keeps running.
- **Single instance**: the host lock from §3. A second launch exits with a message. (Focusing the running
  instance's window is a later nicety.)
- The tray embeds the engine library directly and also serves IPC, so the CLI works alongside it.

Not in M2: autostart at login, installers or bundles, notifications, a settings editor. The config file stays the
source of truth.

## 8. Latency bench (`tools/latency-bench`)

1. Subscribe to the running host's events over IPC.
2. Mutate files in a QA folder at random intervals: create, edit, rename or delete, from a separate Graph client.
3. Match each mutation to the `Progress` event in which the host applied it.
4. Write a CSV (`mutation_at, observed_at, kind, path`) and print p50, p95 and max.

## 9. Testing

- Unit tests:
  - scheduler timing (backoff, jitter bounds, coalescing), using tokio's paused clock
  - pacer rate
  - the IPC round trip
  - history pruning
- Live tests (feature `live`): start a `Service` against the QA tenant, mutate remotely, and wait for `RunFinished`
  with the change applied. Then check that `trigger` over IPC from the CLI binary works.
- Manual: the TUI and the tray, on macOS.

## 10. Decisions

- GUI framework: **Tauri v2** (2026-10-11). Chosen for the mature tray and multi-window support plus the plugins a
  tray sync app needs soon (autostart, notifications, updater).
- The tray is a separate binary, so the CLI never links a webview and stays usable on headless machines.
- Push (socket.io, M3) comes right after M2. At company scale it is what keeps the fleet inside the shared per-app
  budget (`docs/sharepoint/throttling.md`).

## 11. Implementation notes

What was built differs from the plan above in these ways:

- **Sign-in required**: libraries are rechecked every 5 minutes rather than stopping outright. The recheck only
  touches the token endpoint, never SharePoint. `Service::signed_in()` (or the IPC `signed_in` request, which
  `looper-sharepoint login` sends) re-runs them immediately.
- **The keychain token provider resolves the account on every refresh** (`RefreshingProvider::new(authority,
  store)`), so a host started before sign-in, or across a sign-out and sign-in, works without a restart.
- **Sign-in moved into the engine** (`account::sign_in / sign_out / token_provider`) so that every host, and every
  embedding app, shares it. Hosts supply only "open this URL" or "show this code".
- **Dry runs** through the service are not recorded in history and don't change status.
- **The pacer also spaces the second hop of each download** (the pre-authenticated URL Graph redirects to). Delta
  responses don't include `@microsoft.graph.downloadUrl`, even when it is selected explicitly (checked on the QA
  tenant), so every download costs 2 requests.
- **Shutdown waits for a run in progress.** Abandoning a run mid-way rolls back its DB transaction while files may
  already have moved on disk, and the next run would then report those moves as conflicts. Hosts force-exit on a
  second Ctrl-C (CLI) or after 15 s (tray "Quit").
  - **Known issue (pre-existing from M1):** a crash or power loss mid-run has the same effect. Fix it later by
    committing after the move phase, or by journaling moves.
- **Tray specifics:**
  - A `--status` flag opens the status window at launch.
  - On macOS the app becomes a regular app (with a Dock icon) while its window is open, and returns to menu-bar-only
    when the window closes. An accessory app can't bring its window to the front.
  - Config or host-lock errors show in the tray menu instead of crashing. "Open Config File" creates the config from
    `config.example.toml` if it is missing.
  - It honours the same test-only `LOOPER_SHAREPOINT_TEST_USERNAME`/`_PASSWORD` hook as the CLI.
- **The TUI** writes logs to `<data_dir>/logs/looper-sharepoint.log`. The tray writes `<data_dir>/logs/tray.log`.

### Results (QA tenant, macOS, 2026-10-11)

| Check | Result |
|---|---|
| On-demand `sync` against a running host (criterion 4) | A fresh remote edit was on disk 0.72 s after the command started |
| Poll pickup (20 s interval) | A remote edit landed about 21 s later |
| Pacing (1,003 files, 328.7 MB, 8 parallel downloads) | 2,014 requests in 254 s, **0 throttled** (M1 unpaced: 116 s, 19 throttled). All 1,003 hashes match |
| Single host | A second `run` refused with the first one's pid. The tray and `run` exclude each other the same way |
| Sign-in required | A host started with no account reports `sign-in required` and rechecks every 5 minutes |
| Shutdown | SIGINT finishes the run in progress, removes the socket, and keeps history (the offline `status` shows it) |
| Tray | Status window live against the tenant. A CLI `trigger` against the tray host updated the window without interaction |
| Latency bench (criterion 3) | **Pass.** 200 mutations, polling every 45 s: p50 24.1 s, **p95 44.5 s**, max 50.2 s. 0 missed, 0 throttled |

Latency by mutation kind (`tools/latency-bench`, gaps of 2–15 s between mutations, about 29 minutes in total):

| Kind | n | p50 | p95 | max |
|---|---|---|---|---|
| create | 73 | 20.9 s | 43.3 s | 45.4 s |
| edit | 58 | 24.5 s | 43.8 s | 50.2 s |
| rename | 39 | 24.9 s | 46.8 s | 49.2 s |
| delete | 30 | 25.3 s | 44.5 s | 50.2 s |

This is what polling alone can do: latency is spread evenly across the 45 s interval (±10% jitter), plus about 1 s of
sync time. Getting much lower needs push (M3).
