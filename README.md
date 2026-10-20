# looper-sharepoint

A developer-friendly, open-source sync client for SharePoint document libraries, written in Rust for macOS and
Windows. It's configured with a single file and gives enterprises optional controls.

**Status: early spike (milestone M4).** What exists today:

- Sign-in as the user: browser (PKCE) or device code. The refresh token is stored in the OS keychain.
- Discovery of the sites and libraries you can access.
- **Two-way** sync of one or more libraries into local folders (or `direction = "download_only"` for a mirror, and
  `read_only = true` per library).
  - Incremental, using Microsoft Graph delta. Renames and moves are tracked by item ID in both directions, so they
    never re-download or re-upload. A local folder rename is one request in SharePoint.
  - Local edits upload within about 5 s (a folder watcher), in place, so SharePoint keeps the version history.
  - Every download and upload is verified against SharePoint's quickXorHash.
- Docs:
  - [docs/user-guide.md](docs/user-guide.md): setup, configuration, behaviour, recovery and troubleshooting.
  - [docs/spike-report.md](docs/spike-report.md): the spike's results and go/no-go.
  - [docs/packaging.md](docs/packaging.md): building the signed, notarized macOS app (`just package-and-sign`).
- Three ways to run it, all built on one embeddable library ([docs/embedding.md](docs/embedding.md)):
  - `looper-sharepoint sync`: one-shot, like `git pull`.
  - `looper-sharepoint run`: a foreground service with a TUI. It shows whether changes are arriving live or by
    polling, plus recent runs, the last sync and the last failure.
  - `looper-sharepoint-tray`: a menu-bar / notification-area app with a status window.
- **Live updates** ([docs/sharepoint/push.md](docs/sharepoint/push.md)): Microsoft notifies the client of changes
  over socket.io. No server is needed, and it is on by default.
  - Changes arrive within about 30 s, which is how Microsoft batches notifications.
  - When live updates are off (`sync.push = false`) or down, the client says so and polls every 45 s instead.
  - A sync can always be forced: tray "Sync Now", `looper-sharepoint sync`, or `trigger`.
- Polite to SharePoint: requests are paced below the per-user limit, every throttle is honoured, and the client says
  when it is behind ([docs/sharepoint/throttling.md](docs/sharepoint/throttling.md)).
- Safety:
  - Nothing is overwritten without a copy: when both sides change a file, your version is kept as a conflict copy
    (and uploaded).
  - Uploads are conditional on the version last seen, and new files never replace an existing one.
  - A mass-delete guard applies in both directions.
  - A missing or empty local folder never deletes anything in SharePoint.
  - Deletes in either direction are recoverable from the SharePoint recycle bin.

Not yet: Files On-Demand placeholders, installers, admin policy files, or Windows validation (M5). See `specs/` for what's planned.

## Quick start

1. Have a tenant admin register the app. See [docs/admin-setup.md](docs/admin-setup.md); it takes about 15 minutes.
2. Create a config. Start from [config.example.toml](config.example.toml); `looper-sharepoint config path` shows where it goes.
3. Run:

```sh
cargo build --release
./target/release/looper-sharepoint login          # opens a browser; --device-code for headless machines
./target/release/looper-sharepoint libraries      # prints [[library]] blocks to paste into config.toml
./target/release/looper-sharepoint config check   # validates config and resolves each library
./target/release/looper-sharepoint sync --dry-run
./target/release/looper-sharepoint sync
./target/release/looper-sharepoint run            # keep syncing; TUI (s sync, a sync all, r reconnect, p pause, q quit)
```

From another terminal, while a host (`run` or the tray) is running:

```sh
./target/release/looper-sharepoint status         # live: mode (live/polling), state, next check, last sync, last failure
./target/release/looper-sharepoint trigger        # sync now, don't wait
./target/release/looper-sharepoint trigger --reconnect   # reconnect live updates now
./target/release/looper-sharepoint sync           # sync now through the host, and wait (the "git pull")
```

The tray app runs the same service from the menu bar (macOS) or notification area (Windows):

```sh
cargo run --release -p looper-sharepoint-tray     # add `-- --status` to open the status window at launch
```

Only one host runs per machine and user; a second one exits with the first one's pid. Every command accepts
`--json`. `sync` exits with code 1 if any item ended in conflict or error, and 2 on a fatal error.

## Sync behavior

| Situation | Two-way (default) | Download-only or `read_only` |
|---|---|---|
| New or changed in SharePoint | Downloaded to a temp file, hash-verified, moved into place | Same |
| Renamed or moved in SharePoint | Renamed or moved locally; not re-downloaded | Same |
| Deleted in SharePoint, local copy unchanged | Local copy deleted (`on_remote_delete = "keep"` to leave it) | Same |
| Deleted in SharePoint, local copy edited | Uploaded again as a new file | Kept, reported as a conflict |
| New file or folder locally | Created in SharePoint | Kept and reported (`local_orphans = "delete"` for a strict mirror) |
| Edited locally, unchanged in SharePoint | Uploaded in place (keeps version history) | Kept, not uploaded |
| Renamed or moved locally | Renamed or moved in SharePoint; not re-uploaded | Restored at the old path |
| Deleted locally, unchanged in SharePoint | Deleted in SharePoint (recycle bin) | Restored |
| Deleted locally, changed in SharePoint | Restored | Restored |
| Edited on both sides | Your version kept as `name (conflict <host> <date>).ext` and uploaded; SharePoint's version at the original name | Not overwritten; reported |
| A run would delete more than `max_delete_fraction` (default 25%, minimum 10 files), either direction | Refused before changing anything; re-run with `--allow-mass-delete` or "Allow these deletions" in the tray | Same |
| The local library folder is missing or empty | Refused; nothing deleted in SharePoint | Recreated and downloaded |
| OS junk, Office and editor temp/lock/swap files, `sync.ignore` | Never uploaded | n/a |
| Name the local OS can't store | Mapped to a Unicode lookalike, and back on upload (`naming.mode`, `naming.char_map`) | Same |

State lives outside the synced folder, in the OS app-data directory (one SQLite DB per library).
`LOOPER_SHAREPOINT_DATA_DIR` overrides the location.

## Development

```sh
cargo test                                                     # offline unit tests
cargo test -p looper-sharepoint --features live -- --test-threads=1   # end-to-end against the QA tenant (download-only, two-way, read-only; needs .env)
```

`tools/fixtures` seeds, mutates, verifies and cleans test data in the QA library:

```sh
cargo run -p looper-sharepoint-fixtures -- seed runs/mytest --files 200
cargo run -p looper-sharepoint-fixtures -- mutate runs/mytest --edits 5 --deletes 2 --renames 2 --folder-renames 1
cargo run -p looper-sharepoint-fixtures -- verify runs/mytest <local library folder>
cargo run -p looper-sharepoint-fixtures -- clean runs/mytest
```

`tools/latency-bench` measures remote→local latency against a running host (the host must include the prefix). It
records whether the host was live or polling; `--expect-live` fails the run if live updates dropped:

```sh
cargo run --release -p looper-sharepoint-latency-bench -- --prefix runs/latency-bench --mutations 200
```

`looper-sharepoint-fixtures push-probe` logs the raw socket.io traffic for the QA drive; see
[docs/sharepoint/push.md](docs/sharepoint/push.md). For testing the polling fallback, a host started with
`LOOPER_SHAREPOINT_TEST_PUSH_OUTAGE=<file>` behaves as if live updates were unreachable while that file exists.

Layout:

- `crates/looper-sharepoint-core`: config, state DB, name mapping, filters, reconcile rules, quickXorHash. No network code.
- `crates/looper-sharepoint-graph`: a thin Microsoft Graph client with a request pacer, a global throttle gate and error
  classification, plus a minimal socket.io client for live updates.
- `crates/looper-sharepoint-auth`: PKCE, device code, the keychain token store.
- `crates/looper-sharepoint-engine`: the embeddable library: the sync engine plus the long-running `Service`
  (scheduling, status, history, events, sign-in).
- `crates/looper-sharepoint-ipc`: the optional local control socket that hosts serve and the CLI talks to.
- `crates/looper-sharepoint`: the CLI and the `run` TUI host.
- `apps/tray`: the Tauri v2 tray host.

`docs/` describes what exists; `specs/` describes what's planned or in progress. Background on SharePoint
itself lives in `docs/sharepoint/`, for example [throttling limits and how the client stays within them](docs/sharepoint/throttling.md).

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.
