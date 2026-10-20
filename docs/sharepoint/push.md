# SharePoint change notifications over socket.io ("live updates")

This page covers how Microsoft Graph's socket.io notification channel behaves in practice, and how looper-sharepoint
uses it. The facts were measured against the QA tenant on 2026-10-11 with `looper-sharepoint-fixtures push-probe`.
Microsoft documents very little of this, so re-check it with the probe if the behaviour seems to change.

## What it is

```http
GET /drives/{drive-id}/root/subscriptions/socketIo
```

- **Permissions:** delegated only (`Files.Read` or higher); application permissions aren't supported.
- **Response:** a subscription with a `notificationUrl`. Clients connect *out* to that URL, which Microsoft hosts.
  - Nothing is hosted by us, and nothing accepts inbound connections.
  - Graph **webhooks** are the variant that needs a public HTTPS endpoint, and looper-sharepoint doesn't use them.

## Observed behaviour

| Topic | What we saw |
|---|---|
| Endpoint | `https://<region>.pushnp.svc.ms/notifications?token=…&applicationId=…`. The URL path is the socket.io **namespace**; the query string authenticates, so no bearer token is sent |
| Protocol | Engine.IO **v4** (socket.io 3/4) over a websocket. The server offers no upgrades and doesn't support long polling |
| Heartbeat | The server pings every 30 s (`pingInterval: 30000`) with a 20 s timeout. A client that hears nothing for 50 s should treat the connection as dead |
| Subscription | One per user, app and drive: repeated GETs return the same `id` |
| Expiry | One subscription per user, app and library, shared by every client of that user. A GET returns its current `expirationDateTime`. A GET within a few minutes of expiry moves it to 30 min from then; a GET earlier than that leaves it unchanged (seen 2026-10-11: GETs 21 and 16 min before expiry didn't extend it) |
| Silent stalls | Seen once (2026-10-11 soak): a connected socket stopped receiving notifications for 17 min while heartbeats continued. It coincided with another host of the same user quitting, and the subscription's expiry changing as if it had been recreated. Closing one of two sockets on a subscription did **not** reproduce it (`looper-sharepoint-fixtures push-share`). The safety check caught the missed changes |
| Payload | Event `notification` whose argument is a JSON *string* `{"value":[{subscriptionId, clientState, expirationDateTime, resource, tenantId, siteUrl, webId}]}`. It says nothing about what changed; it only means "run delta" |
| Batching | The first change after a quiet period starts a window of about **30 s**. One notification arrives at the end of it, covering every change in the window |
| Delta consistency | A delta call made right after a notification always contained the announced changes |
| Stray notifications | Right after connecting we sometimes got a notification whose delta was empty, probably from changes just before the subscription was renewed. They are harmless: an extra delta costs 1 RU |

### What the batching means for latency

A lone change reaches the client about 30 s after it happens. During continuous activity, changes arrive 10–30 s
after they happen, depending on where in a window they fall. The latency bench (200 random mutations each, details
in `specs/m3-design.md` §10) measured:

| Mode | p50 | p95 | max |
|---|---|---|---|
| Live (push) | 17.5 s | 32.1 s | 34.7 s |
| Polling every 45 s | 26.1 s | 45.9 s | 50.6 s |

Push is faster, and it caps the worst case at about 35 s. It also costs almost nothing in steady state. It can't go
much below 30 s for a lone change, because Microsoft's batching window sets the floor.

## How looper-sharepoint uses it

- **On by default:** `sync.push = true`. With push off (`false`) or down, the client polls delta every
  `sync.poll_interval` (45 s), exactly as without push.
- **One connection per library.** Its state is shown everywhere as **Live**, **Polling** (with the reason) or **off**:
  the `run` TUI's mode bar, the tray icon, the tray menu and status window, and `looper-sharepoint status`.
- **While Live**, scheduled checks slow to `sync.push_safety_interval` (15 min by default), plus one run per
  notification.
- **When the connection drops**, or misses a heartbeat, polling resumes immediately. Reconnects back off from 2 s to
  5 min with jitter.
- **After every (re)connect**, an immediate catch-up delta runs, because notifications sent while the client was
  disconnected are lost.
- **Renewal:** the subscription is renewed by reconnecting with a fresh endpoint 2 min before it expires. The library
  stays Live throughout.
- **Endpoint refused** (401, 403 or 404): the library is marked "unavailable" and retried every 30 min.
- **While throttled, paused, waiting for sign-in, or refused:** no connection is attempted.
- **Reconnect on demand:** tray "Reconnect Live Updates", TUI `r`, or `looper-sharepoint trigger --reconnect`.

The client is in `crates/looper-sharepoint-graph/src/push.rs`, and the state machine is `push_loop` in
`crates/looper-sharepoint-engine/src/service.rs`.

## Cost

The websocket itself (pings and notifications) isn't a Graph request and costs no RU. Steady-state cost per library
while Live:

| Source | Approximate cost |
|---|---|
| Endpoint GET, once per 28 min | ~50 requests/day |
| Catch-up delta after each renewal | ~50 RU/day |
| Safety deltas every 15 min | ~96 RU/day |
| Notification-driven deltas | One per batch of changes |

That's roughly 200 RU a day per library on an idle library, compared with about 1,900 when polling every 45 s. See
[throttling.md](throttling.md) for the budgets.
