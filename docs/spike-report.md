# Spike report: low-latency SharePoint sync (go/no-go)

The spike ran on 2026-10-10 and 2026-10-11, on the QA tenant (`sethcallgmail.sharepoint.com`, one library) from macOS.
The plan and pass criteria are in [specs/spike-plan.md](../specs/spike-plan.md); the details of each milestone are in
`specs/m1-design.md` … `m4-design.md`.

## Verdict: **GO**, for a two-way plain-folder sync on macOS

**What the spike shows works:**
- A config-driven Rust client can keep SharePoint libraries in two-way sync with local folders.
- Changes arrive in seconds, it stays well inside SharePoint's throttling budget, and it has safety rails that
  protect SharePoint from local accidents.
- **Every** criterion the spike set for macOS is met, except the hoped-for push latency (≤10 s). Microsoft's
  notification batching puts that out of reach (see criterion 3).

**Conditions before calling it more than a spike:**
- A person signs in from the tray (the CLI browser sign-in is done).
- The Windows run (M5) passes.
- Real Office applications are checked by hand.

All three are listed under [open items](#open-items).

## Criteria

| # | Criterion | Result | Evidence |
|---|---|---|---|
| 1 | Delegated sign-in, silent refresh, keychain storage | **Partial** (macOS) | See below |
| 2 | Full mirror of a seeded library | **Pass** | 1,003 files in nested folders, including three of ~105 MB. Every file matches its SharePoint quickXorHash (M1; again paced in M2: 2,014 requests, 0 throttled) |
| 3 | Remote → local latency: p95 ≤ 60 s polling; report push | **Pass** (polling); push reported | Polling every 45 s: p50 26.1 s, **p95 45.9 s**. Push: p50 17.5 s, **p95 32.1 s**. 200 changes each, 0 missed (M3) |
| 4 | On-demand sync ("git pull") picks up a change in ≤ ~3 s + transfer | **Pass** | 0.72 s against a running host (M2) |
| 5 | Remote deletes, renames and moves tracked by item id | **Pass** | A folder rename moves its subtree without re-downloading; deletes follow `on_remote_delete` (M1 live test) |
| 6 | Local → remote: uploads with `If-Match`, conflict copies, no data loss | **Pass** | Two-way live test covers 12 scenarios, including conflict copies, mass-delete refusal and a vanished local folder (M4) |
| 7 | Resilience: restart, 410, 429, socket drop | **Pass**, except 410 not exercised; one silent push stall | See below |
| 8 | Request budget matches the model | **Pass**, after a fix the measurement found | Polling 1,870 RU/day per library (model 1,920); live ~190 RU/day. See [throttling.md, Measured cost](sharepoint/throttling.md#measured-cost) |

**Criterion 1 in detail:**
- Token refresh ran unattended for hours in the hosts.
- **Browser (PKCE) sign-in completed by a person** with `looper-sharepoint login` on 2026-10-11: account picker, consent,
  token in the keychain, then `config check` and a sync as that user.
  - A guest account that wasn't a site member got a raw `403 accessDenied`. That now reads "no access to site …: the
    signed-in account isn't a member".
- **Not yet run by a person:** Sign In… in the tray, and the device-code flow.
- Windows not run.

**Criterion 7 in detail:**
- **Restarts:** the state DB carries over.
- **Throttling:** unpaced, the client was throttled 19 times and recovered; with pacing it isn't throttled at all.
- **Socket drop:** falls back to polling within 1 s and catches up on reconnect (M3).
- **Expired sign-in:** reported as "sign-in required" rather than a crash loop.
- **Interrupted runs:** each change now commits as it happens.
- **410 (delta token expired):** the resync path is implemented, but a 410 can't be forced on demand.
- **Soak (4 h, 50 changes, live throughout, 9 renewals, 0 drops):** all 50 changes arrived. 46 arrived in
  29–36 s (p50 31.4 s).
  - **One silent stall:** from 22:48 to 23:05 UTC the socket stayed connected but received no notifications. The
    15-minute safety check found the missed changes at 23:02, so three changes took 4–13 min, and the renewal at
    23:05 restored notifications.
  - It began when another host signed in as the same user quit. That host shared the subscription, which appears
    to have been recreated at that moment. A deliberate repeat (closing one of two sockets) didn't reproduce it.
    See [sharepoint/push.md](sharepoint/push.md).
  - The safety net worked as designed. The cause isn't understood well enough to fix yet, so this is on hold.
  - One machine can't hit this between its own hosts: the tray and `looper-sharepoint run` share a lock, so only one
    runs at a time. The soak host only coexisted with them because it used a separate data folder for testing.

## Latency summary

| Direction | How | p50 | p95 | max |
|---|---|---|---|---|
| SharePoint → local | Polling every 45 s | 26.1 s | 45.9 s | 50.6 s |
| SharePoint → local | Push (live updates) | 17.5 s | 32.1 s | 34.7 s |
| SharePoint → local | `looper-sharepoint sync` against a running host | 0.72 s | | |
| Local → SharePoint | Folder watcher (2 s quiet period) | 3.2 s | 3.6 s | 4.6 s |

Each figure is 200 random creates, edits, renames and deletes, except the on-demand row (a single change). Raw CSVs
come from `tools/latency-bench`.

## What we learned

These were found by measuring, not from the documentation:

1. **Push needs no server, but it's batched.**
   - Graph's socket.io endpoint is a client-outbound websocket (Engine.IO v4), which is great for a desktop client.
   - Notifications arrive about 30 s after the first change in a quiet period, covering everything in that window.
     That window is the latency floor.
   - Subscriptions expire about 30 min after the last fetch, so the client renews them.
   - Details: [sharepoint/push.md](sharepoint/push.md).
2. **Notifications cover the whole library.** A device's push cost scales with activity anywhere in the library, not
   just in its `include` folders.
3. **Delta never returns `@microsoft.graph.downloadUrl`,** so every download is 2 requests.
4. **A folder's eTag doesn't change when its contents change,** so `If-Match` can't protect folder deletes. The client
   lists the folder just before deleting it instead.
5. **Pacing beats reacting.**
   - Spacing requests at 8/s instead of sending them as fast as possible removed throttling entirely.
   - The initial sync of many small files takes about twice as long; steady-state syncing is unaffected.
6. **Measuring the request budget found a 4× overhead** (re-resolving the site and library every check).
   - When polling, that would have hit the tenant's daily cap at about 160 library-devices (≈55 devices with 3
     libraries each).
   - After the fix, the cap is about 640 library-devices when polling, and far more with push.
7. **The mass-delete guard caught a real case:** a test's own cleanup deleting a whole synced folder. Refusing first
   and requiring an explicit "allow" is the right default.

## Open items

| Item | Owner | Why it matters |
|---|---|---|
| Sign In… in the tray (CLI `login` done 2026-10-11) | Seth | The tray's sign-in path has never been run by a person |
| Edit in desktop Word/Excel and in Office Online | Seth | Scripted save sequences pass; real apps and Office Online autosave are unchecked |
| Windows (M5): build and run the suite on `seth-windows` | later | Windows is unvalidated, including credential storage size limits (spec M1 §5) |
| Understand the silent push stall | on hold | Soak: notifications stopped for 17 min with the socket connected. Only matters with several hosts per user (several devices, uncommon) |
| 410 resync under real conditions | opportunistic | It happens only after long disconnections |

## Recommendation

Build on it. The order that makes it usable day to day for a small team:

1. A packaged macOS app: bundle, launch at login, notifications.
2. CI.
3. The open items above.

After that comes the enterprise layer in [specs/policy.md](../specs/policy.md), then Files On-Demand.
