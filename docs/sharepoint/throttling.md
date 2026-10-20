# SharePoint throttling

This page covers what SharePoint Online throttles, how it shows up, and the rules looper-sharepoint follows to stay within
the limits. The figures are Microsoft's published defaults as of October 2026. Microsoft says they "may change at any
time", so check the sources below before relying on exact numbers.

## Core facts

### Limits scale with tenant size

The limits depend on the tenant's **licensed user count**, not on the plan it bought. Every scope below is measured
independently; whichever limit is hit first throttles you.

| Scope | Window | Limit |
|---|---|---|
| Per user, all apps combined | 5 min | 3,000 requests (≈10 req/s) |
| Per user | 1 h | 50 GB ingress, 100 GB egress |
| Per app per tenant | 1 min | 1,250 RU (≤1k licenses) up to 6,250 RU (50k+) |
| Per app per tenant | 24 h | 1.2M RU (≤1k licenses) up to 6M RU (50k+) |
| Per app per tenant | 1 h | 400 GB ingress, 400 GB egress |
| Whole tenant, all apps | 5 min | 18,750 RU (≤1k licenses) up to 93,750 RU (50k+) |

The middle tiers step evenly: 1,001–5,000 licenses doubles the smallest tier, 5,001–15,000 triples it, and so on.

Multi-geo tenants get separate limits per geo, but the license count used for every geo is the tenant-wide total.

### Resource units (RU)

Graph calls have fixed costs:

| RU | Operations |
|---|---|
| 1 | Get a single item; **delta with a token**; download a file |
| 2 | Multi-item queries (list children, delta *without* a token); create, update, delete, upload |
| 5 | Anything touching permissions, including `$expand=permissions` |

Each request inside a JSON batch is charged separately, so batching doesn't save RU. CSOM and REST calls have no fixed
cost and usually cost more than Graph.

### How throttling shows up

- **The response:** HTTP **429** (you exceeded a limit) or **503** (the service is under load). Both carry a
  `Retry-After` header. SharePoint does **not** send IETF `RateLimit` headers, so there is no early warning.
- **Throttled requests still count against the limit.** Retrying before `Retry-After` expires extends the throttle.
  Microsoft says apps that don't honour it "will be blocked due to abusive calling patterns".
- **`Retry-After` can grow if throttling persists.** rclone users have reported pauses of several minutes, and one
  case of more than 33 hours (`Trying again in 120296 seconds`).
- **A block is the worst case:** a persistent 503 for the app or user. Microsoft notifies the tenant through the
  Microsoft 365 Message Center, and the offending process must be fixed before the block is lifted. Microsoft calls
  blocking rare, reserved for "long-term, excessive traffic".
- **Peak hours are stricter.** Throttling is more likely during business hours in the tenant's region. Nights and
  weekends have more headroom.

### All devices share one app budget

Every device in a company signs in through the same app registration (see [admin-setup.md](../admin-setup.md)), so
the **per-app-per-tenant budget is shared by the whole fleet**. Polling cost grows as devices × libraries × polls per day:

| Setup | Daily cost |
|---|---|
| One device polling one library every 60 s | 1,440 RU |
| 300 devices × 3 libraries, polling every 60 s | ≈1.3M RU, which exceeds the 1.2M daily cap for a ≤1,000-license tenant |

When that budget runs out, every user of the app in the tenant is throttled.

Microsoft's guidance for sync engines and other scanners is to use change notifications and call delta when
notified. It warns that "polling the service repeatedly or at high rates causes your app to be throttled", and
recommends a safety-net delta check "no more than once per day" when notifications are in use.

### Workarounds that don't work

- **More app registrations.** Microsoft explicitly warns against registering several app IDs for the same
  application to multiply the per-app budget. They all draw on the same tenant resources, and this has caused
  tenant-wide throttling.
- **Asking Microsoft to raise the limits.** Microsoft won't remove or raise the default thresholds on request.

### What a tenant admin can buy

**SharePoint Service Prioritization** is a pay-as-you-go option (Graph metered APIs, billed through Azure). It gives a
chosen app 2–10× headroom and lets it be throttled last.

- Price: about $0.02 per 1,000 standard calls and $0.14 per 1,000 heavy calls (pricing as of 2025-11-11).
- Setup: SharePoint Online PowerShell, using `New-SPOServicePrioritizationBillingPolicy` and related cmdlets.
- No code changes are needed.
- Extra headroom depends on Microsoft's spare capacity and isn't guaranteed.

### Identify the app's traffic

Microsoft prioritizes traffic that identifies its app, using a registered app ID and a User-Agent in the form
`ISV|Company|App/Version`, over traffic that doesn't.

## Architectural principles

These are the rules that keep looper-sharepoint inside the limits. Each one says whether it is implemented today or
planned. Planned items are tracked in `specs/`.

1. **Honour `Retry-After`, everywhere at once.** *(Implemented, M1.)* One 429 or 503 pauses **every** request in the
   process until the `Retry-After` deadline; workers never each retry on their own schedule. With no header, the client
   backs off exponentially. Requests are retried a bounded number of times, then reported as errors. See
   `crates/looper-sharepoint-graph/src/client.rs`.

2. **Always identify ourselves.** *(Implemented, M1.)* Every request carries
   `User-Agent: ISV|looper-sharepoint|looper-sharepoint/<version>` and comes from a registered app ID.

3. **Use delta with a token for change detection, never folder walks.** *(Implemented, M1; per-check overhead
   removed after measuring.)*
   - A delta call with a token is the cheapest call there is (1 RU), and an unchanged library costs exactly one call.
   - Until the cost measurement below, every run also re-resolved the site and library (a site read plus a drive
     list, 3 RU), so each check cost 4 RU. The resolution is now cached per library and redone only when the config
     changes or the drive disappears.
   - We only fetch the full listing again when the server forces a resync (410).
   - Renames and moves come from delta, so files are never re-downloaded to handle them.

4. **Never read permissions casually.** *(Implemented by omission.)* Permission reads cost 5 RU, and we never send
   `$expand=permissions` in sync paths. Any future feature that needs permissions must budget for that.

5. **Pace below the per-user limit instead of reacting to it.** *(Implemented, M2.)* A client-side pacer spaces every
   request, including both hops of a download, at `sync.max_requests_per_second` (default 8, maximum 10). That stays
   under the 10 per second the 3,000-per-5-minutes limit allows.
   - **Measured:** unpaced, the M1 scale test (1,003 files) reached about 17.5 requests per second and was throttled
     19 times in 116 s. Paced at 8, the same sync made 2,014 requests in 254 s and was throttled 0 times.
   - **Cost:** the first sync of many small files takes about twice as long. Steady-state polling is unaffected.
   - **Known gap:** the budget is per user, not per process, so one user syncing on several machines still shares it.
     Microsoft calls out exactly that case, and no client-side setting fully covers it.

6. **Push first, poll as a fallback.** *(Implemented, M3; see [push.md](push.md).)* At company scale the shared
   per-app daily budget, not latency, is the binding limit.
   - Each device keeps a socket.io connection open per library (`sync.push`, on by default) and calls delta when
     notified.
   - While a library is Live, its scheduled checks slow to `sync.push_safety_interval`.
   - Fixed-interval polling takes over whenever push is off or down, and is fine for small teams.
   - **Measured:** an idle Live library costs about 200 RU/day, compared with about 1,900 polling every 45 s.

7. **Adapt the polling interval.** *(Mostly implemented, M2–M3: jitter, failure backoff, pushing the next poll past
   a throttle window, and the push-dependent intervals. Faster polling after activity is not done yet.)*
   - Push connected: a slow safety-net delta (`push_safety_interval`, default 15 min). Large fleets should raise it
     toward hourly; Microsoft suggests as little as once a day.
   - Push down: poll at the configured interval, starting immediately.
   - After a push event or local activity: poll faster for a short period, then decay back.
   - While throttled: lengthen the interval as well as obeying `Retry-After`.
   - Add jitter so a fleet of devices never polls in lockstep.

8. **Let admins cap fleet cost.** *(Planned, policy.)* A lockable policy knob sets a minimum poll interval, and
   optionally a maximum download concurrency. Admins can then size the fleet's cost against their tier with the
   table above (devices × libraries × polls per day ≪ the daily RU cap). See [specs/policy.md](../../specs/policy.md).

9. **Show the user that the client is behind, honestly.** *(Implemented, M2: `status`, the `run` TUI and the tray all
   show the throttle deadline, the last successful sync and the last failure with its error.)* A throttle pause isn't idle.
   `status`, and later the tray, must report the state, such as "throttled until 14:32; last complete sync at
   14:05". They must also report a long-running backlog so users know local files may be stale. Under SharePoint's
   rules a long pause can't always be avoided, so making it visible is the remedy.

10. **Keep big transfers off peak hours (optional).** *(Planned, later.)* A large initial sync or a re-hydration can
    optionally defer the bulk of its downloads to off-peak hours in the tenant's time zone.

11. **One app registration per tenant.** *(Documented.)* Never advise splitting the fleet across several app IDs to
    get more budget; see "Workarounds that don't work". The supported way to buy headroom is Service Prioritization,
    an admin decision.

## Measured cost

The client estimates its own usage, because SharePoint doesn't report it. Every request is classified with the costs
above, and `looper-sharepoint status` shows the total and a per-day rate (see `RequestKind` in
`crates/looper-sharepoint-graph/src/client.rs`). It counts every attempt, including retries and throttled requests,
and counts a download's redirect hop as a second request.

Measured on the QA tenant on 2026-10-11, one library per host:

| Situation | Measured | Notes |
|---|---|---|
| Idle, polling every 45 s | **1,870 RU/day** | One delta per check. The model is 1,920. Before the fix above it was 7,400 |
| Idle, live (push) | **~190 RU/day** in a quiet library | 96 safety deltas (15 min), about 48 endpoint renewals and 48 catch-up deltas |
| ...plus activity in the library | +1 RU per notification batch | Notifications cover the whole library, not just your `include` folders. One batch at most every ~30 s, so a constantly busy library costs a live device at most about as much as polling every 30 s. The measured 575 RU/day had another test editing the library every ~5 min |
| Each change made in SharePoint | **~1.5 RU** | Download (1 RU for the call + 1 for the redirect hop) plus a share of a batched delta |
| Each local change uploaded | **~4.8 RU** | Upload session + one chunk (2 RU each), plus that run's delta |

**Fleet sizing with these numbers** (≤1,000-license tenant, 1.2M RU/day per app):

| Fleet | Daily cost |
|---|---|
| 3 users × 3 libraries, live, each making 200 changes/day | ≈ 9 × 190 + 600 × 4.8 + downloads ≈ **5–10k RU** |
| 300 devices × 3 libraries, live, quiet libraries | ≈ **170k RU** |
| 300 devices × 3 libraries, polling every 45 s | ≈ **1.7M RU**, which exceeds the daily cap. Push (on by default) is what makes a fleet that size fit |

## Sources

- [Avoid getting throttled or blocked in SharePoint Online](https://learn.microsoft.com/en-us/sharepoint/dev/general-development/how-to-avoid-getting-throttled-or-blocked-in-sharepoint-online) (Microsoft Learn, updated 2026-08)
- [Best practices for discovering files and detecting changes at scale](https://learn.microsoft.com/onedrive/developer/rest-api/concepts/scan-guidance?view=odsp-graph-online) (Microsoft Learn)
- [Microsoft Graph throttling limits](https://learn.microsoft.com/graph/throttling-limits)
- [SharePoint Service Prioritization](https://learn.microsoft.com/en-us/sharepoint/sharepoint-prioritization)
- [Data migration and SPO service throttling](https://learn.microsoft.com/en-us/archive/blogs/sposupport/data-migration-and-spo-service-throttling) (archived Microsoft support blog)
- [Veeam KB4855: impact of Service Prioritization](https://www.veeam.com/kb4855)
- rclone forum: [pacer sleep time skyrockets](https://forum.rclone.org/t/onedrive-rclone-getting-throttled-and-pacer-sleep-time-skyrockets-to-5m/26815/4), [throttling with SharePoint, saga continues](https://forum.rclone.org/t/throttling-of-rclone-with-sharepoint-saga-continues/29874)
