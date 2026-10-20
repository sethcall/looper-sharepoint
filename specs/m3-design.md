# M3 Design: live updates (socket.io push) with a polling fallback

Status: **implemented, pending sign-off** (see §10 for what the probe found and where the build differs from this plan). Builds on `specs/m2-design.md` (the service, hosts and scheduler) and principles 6 and 7
of `docs/sharepoint/throttling.md`.

## 1. Goal

Libraries update within seconds of a change in SharePoint, without anyone running a server.

- **Push is on by default.** Each library opens a socket.io connection to Microsoft Graph and runs a delta as soon as
  Graph says something changed.
- **Push is optional.** `sync.push = false` turns it off. When push is off or can't connect, the client is
  **polling**: it behaves exactly as M2 does, running a delta every `poll_interval` (45 s by default). That is
  slower, but nothing breaks.
- **The mode is always visible.** The TUI, the tray icon and menu, the status window and `looper-sharepoint status` all
  show clearly whether each library is **Live** or **Polling**, and why.
- **A delta can always be forced by hand**, whatever the push state: tray "Sync Now", `looper-sharepoint sync` /
  `trigger`, or TUI `s`/`a`. These all exist since M2.

### No server needed

`GET /drives/{drive-id}/root/subscriptions/socketIo` (delegated, `Files.Read` or higher) returns a `notificationUrl`.
The client connects *out* to that URL, which Microsoft hosts. Nothing listens for inbound connections, and nothing
needs hosting. Graph **webhooks** are the variant that needs a public HTTPS endpoint, and M3 doesn't use them.

### Exit criteria

- **Criterion 3 (push):** report remote→local p50 and p95 over ≥200 mutations with push on. The target is p95 ≤ 10 s.
  Re-run with `push = false` to confirm p95 stays ≤ 60 s.
- **Criterion 7 (resilience):** when the socket drops, the library goes to Polling within one ping timeout, and
  catches up with a delta once it reconnects. Run for at least 4 hours with no stuck states.
- **Mode visibility:** the push state shows in all four places listed in the goal, and each transition is visible
  within 1 s.

## 2. Modes

Each library is in exactly one mode.

| Mode | Shown as | When | Scheduled delta |
|---|---|---|---|
| **Live** | `● Live` (green) | Push connected | Every `push_safety_interval` (default 15 min), plus on every notification |
| **Connecting** | `◌ Connecting…` | First connect, or reconnecting after a drop | Every `poll_interval` |
| **Polling** | `◐ Polling every 45s` (amber) + reason | Push enabled but down: network, endpoint error, throttled, retry pending | Every `poll_interval` |
| **Polling (push off)** | `○ Polling every 45s · live updates off` (neutral) | `sync.push = false` | Every `poll_interval` |

Rules:

- **Polling is the safe default.** Any time the client isn't sure push is working, it polls at `poll_interval`.
  Push only ever *slows* the timer, and only while it is connected.
- **Catch-up on (re)connect.** A successful connect triggers an immediate delta, because notifications sent while
  the client was disconnected are lost.
- **Drop → Polling immediately.** When the socket closes or misses its heartbeat, the next scheduled delta moves to
  `min(next_run_at, now + poll_interval)`. If the last delta is already older than `poll_interval`, it runs now.
- **Sign-in required, paused, or mass-delete refused:** the push connection closes, because no runs happen anyway. It
  reopens when the library becomes runnable again.

## 3. Push lifecycle

One push task per library runs alongside its scheduler task (M2 §4).

```
          ┌──────────── backoff (2s → 5 min, ±20% jitter) ────────────┐
          ▼                                                            │
  [get endpoint] ── GET …/root/subscriptions/socketIo ──► [connect] ──┤ error
          │                                                    │       │
          │ 401/403/404 on endpoint                            ▼       │
          ▼                                              [Live] ── drop / heartbeat miss
  [Polling: "live updates unavailable"]                        │
   retry every 30 min                                          └─ notification → debounce 500 ms → Trigger::Push
```

- **Endpoint:** fetch a fresh `notificationUrl` on every connect attempt; don't cache it. The GET goes through the
  Graph client, so it obeys the pacer and the throttle gate. While throttled, the library shows Polling with the
  reason "throttled".
- **Transport:** websocket only, because Graph doesn't support long polling. The socket's own traffic (pings,
  notifications) isn't a Graph request and costs no RU.
- **Heartbeat:** Engine.IO pings. If nothing arrives within `pingInterval + pingTimeout`, the connection is dead.
- **Notification:** the payload is treated only as "this drive changed". Start a run 500 ms after the first
  notification, so a burst (e.g. a folder upload) collapses into one delta. Further notifications during a run queue
  one follow-up run, using the existing M2 coalescing.
- **Backoff:** 2 s, 4 s, 8 s … up to 5 min, ±20% jitter, reset after a connection that stayed up 5 min. An endpoint
  answer of 401, 403 or 404 means push isn't available for this drive, tenant or account. That is reported as such
  and retried only every 30 min.
- **Manual reconnect:** "Reconnect live updates" (tray), `r` (TUI) or IPC `reconnect` skips the backoff wait.

## 4. Config

New `[sync]` keys:

| Key | Default | Notes |
|---|---|---|
| `push` | `true` | `false` = polling only, with no socket connections |
| `push_safety_interval` | `"15m"` | Delta interval while Live. Min `"1m"`. Large fleets should raise it toward hourly (throttling.md, principle 7) |

`poll_interval` (45 s) keeps its meaning: the interval whenever a library isn't Live. A future admin policy can force
`push = true` and raise the minimum `poll_interval`.

**Budget (3 users × 3 libraries, ≤1k-license tenant cap of 1.2M RU/day):**

| Mode | Cost |
|---|---|
| All Live | 9 × 96 safety deltas ≈ 860 RU/day, plus 1 RU per notification-driven delta and 1 per reconnect |
| All Polling | 9 × 1,920 ≈ 17,000 RU/day |

Both are far under the cap. Push matters for latency at this size and for budget at fleet size.

## 5. Status and UI

### Library API (`looper-sharepoint-engine`)

```rust
pub enum PushState {                       // serde tag "state"
    Off,                                   // sync.push = false
    Connecting { attempt: u32 },
    Live { since: DateTime<Utc> },
    Down { since: DateTime<Utc>, reason: String, retry_at: Option<DateTime<Utc>> },
    Unavailable { reason: String, retry_at: DateTime<Utc> },   // endpoint refused (401/403/404)
}
LibraryStatus.push: PushState
LibraryStatus.last_push_at: Option<DateTime<Utc>>   // last notification received
Status::mode() -> Mode                              // Live | Mixed | Polling | PollingByChoice
Service::reconnect_push(lib: Option<&str>)
```

- Push transitions emit `Event::StatusChanged`.
- `Health` is unchanged. Polling isn't an error, so it never turns the icon to "attention" by itself.
- IPC: new `reconnect { library }` request. `status` responses carry `push` per library.

### TUI: the mode in the header, plus a LIVE column

```
 looper-sharepoint ─ spqa1@sethcall.com ─ ● LIVE · updates arrive in seconds ──────────────────
  LIBRARY   STATE    LIVE UPDATES             LAST SYNC   LAST FAILURE   NEXT CHECK
 ▶ qa        idle     ● live for 2h14m          4s ago      never          safety check in 11m
   legal     idle     ◐ reconnecting in 12s     38s ago     never          in 7s
                        (connection lost 2m ago: heartbeat timeout)
```

- **Header badge:** `● LIVE` (green), `◐ POLLING every 45s — live updates down: <reason>` (amber), `◐ PARTLY LIVE`
  when libraries differ, or `○ POLLING every 45s — live updates off in config` (dim).
- **Detail panel:** the selected library's push history (connected at, last notification, last drop and reason).
- **New key:** `r` reconnects live updates.

### Tray

- **Icon:**
  - Live and ok: the existing `ok` icon.
  - Polling while push is enabled but down: a new **`polling`** variant, an outlined version of `ok`. You can see at a
    glance that it is working but not live.
  - `Off` uses the normal `ok` icon, since it is a deliberate choice; the menu still says so.
  - Syncing, waiting and attention keep their M2 priority.
- **Menu**, as a second disabled line under the headline:
  - `● Live updates on`
  - `◐ Live updates down (2m) · checking every 45s`
  - `○ Live updates off · checking every 45s`
  - Per-library lines get a `●`/`◐`/`○` prefix. Adds "Reconnect live updates" when any library is down.
- **Status window:** a mode pill at the top, next to the headline, with the same three wordings plus the reason. Each
  library card shows its push state and "last notification 3s ago".

### `looper-sharepoint status`

Prints a `mode:` line first, and a `live updates:` row per library. `--json` includes `push`.

## 6. Implementation

- **`looper-sharepoint-graph`:**
  - `GraphClient::socket_io_endpoint(drive_id) -> Url`
  - a new `push` module: a small Engine.IO v4 / socket.io client over `tokio-tungstenite` (rustls), covering
    connect, namespace join, ping/pong, event decoding and close
  - `push::connect(url) -> PushStream` yields `Notification` and `Closed(reason)`
- **Why hand-rolled instead of `rust_socketio`:**
  - We need only a few hundred lines: one event name, receive only, websocket only.
  - It avoids a second HTTP/TLS stack.
  - We get exact control over heartbeats and errors, which drive the visible state.
  - If the probe (§7, step 1) shows protocol quirks, a crate doesn't save us anyway.
- **`looper-sharepoint-engine`:**
  - `push_loop` per library (the state machine of §3)
  - `PushState` held in the shared status
  - `Trigger::Push` (reserved in M2) is now used
  - `next_delay` takes the push state into account
  - a `Notify` lets push transitions reschedule the timer
- **Hosts:** the TUI header, column and `r` key; the tray icon variant, menu lines, pill and reconnect item; the CLI
  `status` output.
- **`tools/latency-bench`:** report the mode at start and end, and fail if push dropped during a push-on run, unless
  `--allow-drops` is given.
- **Docs, on ship:**
  - `docs/sharepoint/push.md`, covering what we learned about the endpoint
  - throttling.md principles 6 and 7 → implemented
  - README, `docs/embedding.md`, `config.example.toml`

## 7. Plan

1. **Probe (do first; it may change §3 and §6).** A `push-probe` command in `tools/fixtures` fetches the endpoint for
   the QA drive, connects, and logs raw frames while the fixtures tool mutates files. It answers:
   - Protocol: EIO=4 or EIO=3? Is the URL path the socket.io namespace (the docs' "split on `/callback?`")?
   - Payload: what does a notification contain? One per change, or batched? Is there any echo for our own writes
     (this matters for M4 uploads)?
   - Lifetime: does the endpoint or socket expire (the probe stays open for 4 hours or more)? Does the server
     disconnect idle sockets?
   - Latency: time from a Graph write to the notification, and to the change being visible in delta. If a
     notification can arrive before delta shows the change, the delta right after it would miss it; then also run a
     follow-up delta about 5 s later.
2. `socket_io_endpoint` plus the push client, with unit tests against a local fake Engine.IO server: handshake,
   ping/pong, event, server close, heartbeat timeout.
3. The engine push loop and the scheduler changes, with paused-clock tests:
   - Live slows the timer
   - a drop restores `poll_interval`
   - reconnect triggers a catch-up delta
   - notification bursts are debounced
   - push is off when `push = false`
4. Status, IPC and the CLI `status` output.
5. The TUI and the tray.
6. Live runs:
   - the bench with push on and with push off
   - a forced drop (IPC `reconnect`, and toggling network), checking the fallback and catch-up
   - a 4-hour soak
   - an edit saved from Office Online, to check autosave lag (manual)

## 8. Open questions (answered by the probe)

- The protocol details and endpoint lifetime in §7 step 1.
- Whether one socket per library is acceptable, or sockets on the same host should be shared. Per library is
  simplest and fine at 3 users × a few libraries; revisit for fleets.
- Whether the safety interval should drop automatically when notifications look unreliable, e.g. a safety delta finds
  changes that no notification announced. Proposal: log it in M3, decide with data.

## 9. Not in M3

- Webhooks (they need a server).
- A self-hosted relay.
- Echo suppression for our own uploads (M4).
- OS notifications for push state.

## 10. Implementation notes

The probe (§7 step 1) answered §8's questions; [docs/sharepoint/push.md](../docs/sharepoint/push.md) records them. Two
findings changed the plan:

- **Latency: the ≤ 10 s target isn't reachable with push.**
  - Graph batches notifications: the first change after a quiet period is announced about 30 s later, with
    everything else in that window.
  - Measured (below): push p95 is 32 s, compared with 46 s polling every 45 s. A lone change still takes about 30 s.
  - Its main win is cost: an idle Live library costs about 200 RU/day, compared with about 1,900 when polling.
  - The 500 ms debounce in §3 was dropped. Notifications already arrive at most about once per 30 s, and the
    scheduler coalesces anything that arrives during a run.
- **Subscriptions expire about 30 min after the last GET**, and a GET before expiry extends them. (Corrected
  2026-10-11: only a GET within a few minutes of expiry extends it; see docs/sharepoint/push.md.) The push task
  reconnects with a fresh endpoint 2 min before expiry. The library stays Live through the swap; it doesn't drop to
  polling. A catch-up delta still runs afterwards, at 1 RU per renewal.

Other differences from the plan:

- **Paused, sign-in required or refused** libraries show push as `down` with no `retry_at`. Hosts render that as
  "paused: <reason>". The tray icon doesn't switch to the polling outline for these states; the headline already
  covers them.
- **Test hook:** a host started with `LOOPER_SHAREPOINT_TEST_PUSH_OUTAGE=<file>` treats live updates as unreachable
  while that file exists.
  - Why: SIGSTOP is a no-op in the dev sandbox, and cutting the network would disrupt the whole machine.
  - The hook drives the real drop, fallback, backoff and reconnect paths. Only the cause of the drop is simulated.
- **Reconnect from the CLI** is `looper-sharepoint trigger --reconnect`, not a new subcommand.
- **Scheduler tests:** the paused-clock tests in §7 step 3 became unit tests of the interval rules (`next_delay`,
  `push_backoff`, `Status::mode`) plus the live runs below. Driving `push_loop` against a fake Graph would need the
  Graph base URL to be injectable; that's deferred.

### Results (QA tenant, macOS, 2026-10-11)

| Check | Result |
|---|---|
| First connect | Live about 2 s after start, followed by a catch-up delta. The next safety check moved out to 15 min |
| A remote write while Live | On disk 31 s after the write, via one notification |
| Subscription renewal | At expiry − 2 min. The new expiry was +30 min, and the library stayed Live |
| Simulated outage | Polling within 1 s. The next check was pulled in from 14 min to 37 s. Reconnect backoff 5 s → 9 s → 19 s. A file written during the outage arrived by polling after 17 s |
| `trigger --reconnect` after the outage | Live again in under 3 s, with a catch-up delta |
| Tray | Mode strip and per-library state in the status window. The icon is filled when Live and outlined when polling. The CLI reconnected the tray's host over IPC |

Latency bench (200 mutations each, run at the same time against two hosts):

| Mode | p50 | p95 | max | Missed | Runs | Notes |
|---|---|---|---|---|---|---|
| Push on (`--expect-live`) | 17.5 s | 32.1 s | 34.7 s | 0 | 55 push-triggered, 0 polls | Live for the whole run, including a subscription renewal at 18:23:33. 464 Graph requests, 0 throttled |
| Push off | 26.1 s | 45.9 s | 50.6 s | 0 | 40 polls | Matches M2's polling result (p95 44.5 s) |

Per kind with push on, p95: create 32.1 s, edit 32.4 s, rename 29.8 s, delete 31.2 s.

**Exit criteria:**
- Criterion 3 (push): measured and reported. p95 32 s is well inside the 60 s bar, but misses the hoped-for ≤ 10 s
  for the batching reason above.
- Criterion 7: drop → polling → reconnect → catch-up was verified with the outage hook.
- Push visibility: in all four places.
- **Not yet done:** the 4-hour soak. The longest continuous run so far is about 35 min, with one renewal; a probe
  stayed connected for 25 min. Also outstanding: the Office Online autosave check (manual).
