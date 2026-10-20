# Embedding the sync engine in your app

`looper-sharepoint-engine` is the whole sync client as a Rust library. It covers sign-in, scheduling, throttling
discipline, state, run history, status and events. Your app supplies the UI and process lifecycle. The engine never
opens windows, daemonizes, handles signals, or writes to the terminal.

The two shipped hosts use only this public API:

| Host | What it adds |
|---|---|
| `looper-sharepoint run` | A ratatui TUI, or log lines with `--no-tui` (`crates/looper-sharepoint`) |
| `looper-sharepoint-tray` | A Tauri v2 menu-bar / notification-area app with a status window (`apps/tray`) |

```toml
[dependencies]
looper-sharepoint-engine = { git = "https://github.com/sethcall/looper-sharepoint" }
# Optional: let the `looper-sharepoint` CLI observe and drive your app (status, trigger, sync):
looper-sharepoint-ipc = { git = "https://github.com/sethcall/looper-sharepoint" }
```

The engine re-exports its building blocks as `looper_sharepoint_engine::{core, auth, graph}`, so it is the only
dependency you need.

## Start a service

```rust
use looper_sharepoint_engine::{Service, ServiceConfig, account, core::config::{Config, data_dir}};

let config = Config::load(&Config::default_path())?;     // or Config::parse(text, path) from your own storage
let data = data_dir();                                    // or a directory your app owns
let tokens = account::token_provider(&config, &data);     // keychain-backed; works before sign-in
let svc = Service::start(ServiceConfig { config, tokens, data_dir: data })?;   // inside a tokio runtime
```

`Service::start`:

- takes an exclusive lock on the data dir. Only one host can sync a data dir at a time; a second gets
  `LockError::Held` with the other process's pid.
- loads each library's run history.
- starts one scheduler per library. Each library syncs once immediately.

**Changing the config while running:** `svc.reload(new_config).await` lets runs in progress finish, then sets every
library up again from `new_config` (each syncs once). State, pause, history and event subscribers carry over. A
different tenant or app is refused. A host that keeps its config in a file calls `svc.set_config_file(path)`; then
`svc.reload_file()` re-reads it, and so does the CLI's `reload` request (sent by `add`, `remove` and `setup`).
- with `sync.push` (the default), opens a live-updates connection per library
  ([docs/sharepoint/push.md](sharepoint/push.md)).
  - While a library is Live, it runs on each change notification, plus a safety check every
    `sync.push_safety_interval`.
  - Otherwise it polls every `sync.poll_interval` (±10% jitter).
- with two-way sync and `sync.watch` (the defaults), watches each library folder and runs about 2 s after local
  changes settle (`Trigger::Local`). Paths the engine itself writes are ignored.
- backs failed runs off exponentially, up to 30 minutes.

`Service` is cheap to clone. Every clone controls the same service.

## Render status

```rust
let status = svc.status();         // Status: account, throttled_until, paused, libraries[...]
status.headline();                 // "Up to date", "Sign-in required", "2 libraries failing to sync", ...
status.health();                   // Health::{Ok, Syncing, Waiting, Attention}, e.g. to pick an icon
status.mode();                     // Mode::{Live, PartlyLive, Connecting, Polling{since, reason}, Off}: show it prominently
status.mode().describe(status.poll_interval_secs);   // "Live updates on", "Live updates down · checking every 45s", ...
let runs = svc.history(20);        // recent RunRecords across libraries, newest first
```

Each `LibraryStatus` has:

| Field | Contents |
|---|---|
| `state` | `idle`, `syncing{done,total}`, `error`, `sign_in_required` or `refused` |
| `last_success_at` | Time of the last successful run |
| `last_failure` | `{at, error}` |
| `next_run_at` | When the next scheduled run starts |
| `last_run` | The most recent `RunRecord`, including up to 20 conflicts or errors |
| `push` | Live updates: `off`, `connecting`, `live{since}`, `down{since, reason, retry_at}` or `unavailable{…}` |
| `last_push_at` | When the last change notification arrived |
| `uploads` / `watching` | Whether local changes are uploaded, and whether they sync within seconds (watcher) or at the next check |

Every type is plain serde, so you can hand it to a webview or send it over IPC as JSON unchanged.

## React to events

```rust
let mut events = svc.subscribe();
while let Ok(ev) = events.recv().await {
    match ev {
        Event::RunFinished { record } => { /* maybe notify the user */ }
        Event::Progress { library, progress } => { /* per file: Downloaded, Uploaded, Moved, MovedRemote, Deleted,
                                                     DeletedRemote, ConflictCopy, Conflict, Error */ }
        Event::Throttled { until } => { /* SharePoint is throttling; everything waits until `until` */ }
        _ => {}
    }
    redraw(svc.status());
}
```

The channel is lossy for slow subscribers (`RecvError::Lagged`). Treat events as "something changed" and re-read
`status()`. A large sync emits one `Progress` event per file, so coalesce redraws.

## Control

| Call | Effect |
|---|---|
| `svc.trigger(None \| Some("lib"), Trigger::Manual)` | Run now. Returns immediately; coalesces with a run in flight |
| `svc.sync_now(lib, RunOptions { dry_run, allow_mass_delete }).await` | Run now and wait for the `RunResult`s, including full reports. After a `refused` run, `allow_mass_delete: true` is the "Allow these deletions" action |
| `svc.reconnect_push(None \| Some("lib"))` | Reconnect live updates now, skipping the reconnect backoff |
| `svc.pause()` / `svc.resume()` | Stop or restart scheduled runs, and close live updates. Manual runs still happen |
| `svc.signed_in()` | Call after a sign-in so libraries waiting on it run immediately |
| `svc.shutdown().await` | Stop. A run in progress finishes first, which keeps the state DB and disk consistent |

## Sign-in

```rust
use looper_sharepoint_engine::account::{self, SignInPrompt};

let acct = account::sign_in(&config, &data, None, |prompt| match prompt {
    SignInPrompt::OpenUrl(url) => open_in_browser(url),           // PKCE with a loopback redirect
    SignInPrompt::DeviceCode(info) => show_code(&info.message),   // device code flow
}).await?;
svc.signed_in();
```

The refresh token goes to the OS keychain. `account::sign_out` forgets it. When the refresh token expires or is
revoked, libraries move to `sign_in_required` and recheck every 5 minutes. That recheck only contacts the sign-in
endpoint, never SharePoint.

## Control socket (optional)

```rust
let _server = looper_sharepoint_ipc::serve(svc.clone())?;   // keep it alive as long as the service
```

Then `looper-sharepoint status`, `trigger` (including `--reconnect`) and `sync` work against your app. On macOS and Linux the socket is
`<data_dir>/host.sock` with mode 0600; on Windows it is a named pipe. The protocol is newline-delimited JSON; see
`crates/looper-sharepoint-ipc`.

## One-shot use

If you don't want scheduling at all, use `sync_library(&graph, &config, &data_dir, &library, &RunOptions::default(),
&no_progress)`. Hold a `HostLock` on the data dir while you do.
