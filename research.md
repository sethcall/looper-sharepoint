# SharePoint Sync — Research Findings (2026-10-10)

Goal: an open-source, developer-friendly OneDrive equivalent in Rust (Windows + macOS) that syncs SharePoint
document libraries with git-pull-like latency, driven by a config file, with optional enterprise controls.

## TL;DR

- **Nothing to fork.** There is no maintained open-source Rust SharePoint/OneDrive sync client. We'll write
  the sync engine ourselves. Crates exist for auth, basic Graph calls, and the OS integration bindings.
- **The 1-minute latency target looks achievable.** Push via Graph's **socket.io channel** (GA, no public endpoint
  needed, delegated auth only) → triggers a **delta** query. A **45s delta poll** backs it up and enforces the
  worst case. An on-demand `sync` command runs delta immediately. A delta call costs 1 RU when nothing changed.
- **Biggest unknown:** Microsoft publishes no latency numbers for delta or socket.io. The spike must measure them.
- **QA tenant:** Use the free M365 E5 Developer sandbox if we qualify (Visual Studio Pro/Enterprise subscription or
  Partner Action Pack). If not, start a new tenant on a Business Basic trial (~$6–7/user/mo). Use the
  production jamkazam.com tenant only for site-scoped read-only smoke tests.
- **Admin consent will be needed** in most tenants. Since Aug 2025, Microsoft's managed consent policy blocks user consent to
  `Files.*.All`/`Sites.*.All`. Design for bring-your-own app registration and `Sites.Selected` from day one.

## 1. Open-source landscape

| Project | Lang / License | Status | Use to us |
|---|---|---|---|
| [rclone](https://github.com/rclone/rclone) onedrive backend | Go / MIT | Very active | Reference: QuickXorHash, throttling, upload sessions, SharePoint drive discovery |
| [abraunegg/onedrive](https://github.com/abraunegg/onedrive) | D / GPL-3 | Very active | Reference only (GPL): best map of Graph/SharePoint edge cases; already uses socket.io push. Linux only |
| [nextcloud/desktop](https://github.com/nextcloud/desktop) + [NextcloudFileProviderKit](https://github.com/nextcloud/NextcloudFileProviderKit) | C++ / Swift, GPL | Active | **Reference architecture**: sync journal DB, discovery/propagation split, CfAPI + File Provider placeholders |
| [Unison](https://github.com/bcpierce00/unison) | OCaml | Active | 3-way archive / reconciliation semantics |

**Rust building blocks**

- Graph: [`onedrive-api`](https://github.com/oxalica/onedrive-api) (MIT, delta + upload sessions; small, single maintainer, so fork or vendor it).
  [`graph-rs-sdk`](https://github.com/sreeise/graph-rs-sdk) is a heavier fallback and its development is slowing. There's no official Microsoft Rust Graph SDK. Expect a thin
  reqwest layer of our own for sites, lists, and subscriptions.
- Auth: [`oauth2`](https://github.com/ramosbugs/oauth2-rs) (auth-code + PKCE on a loopback redirect, device code) plus [`keyring`](https://crates.io/crates/keyring)
  for refresh tokens. [`azure_identity`](https://crates.io/crates/azure_identity) has confidential/app-only credentials only. Avoid the `msal` crate.
- Push: [`rust_socketio`](https://crates.io/crates/rust_socketio). Untested against Graph's endpoint — **spike risk**.
- Windows placeholders (later): [`cloud-filter`](https://github.com/ho-229/cloud-filter-rs) / [`wincs`](https://github.com/ok-nick/wincs) (immature) or raw `windows-rs` CloudFilters.
- macOS placeholders (later): `objc2-file-provider` raw bindings only. The practical route is a thin Swift `.appex` over a Rust core (uniffi/XPC).
- Other: `notify` (FS watching), `rusqlite` (state DB), `tokio`.

**We write:** sync engine (base/local/remote state, reconciliation, conflicts, ID-based rename/move, resumable
transfers), change-trigger layer, multi-site config + discovery, token cache, enterprise policy, macOS packaging.

## 2. Graph API mechanics & latency

**Delta** ([docs](https://learn.microsoft.com/en-us/graph/api/driveitem-delta?view=graph-rest-1.0), [scan guidance](https://learn.microsoft.com/en-us/onedrive/developer/rest-api/concepts/scan-guidance))
- One cursor per drive (library). There's no site-wide delta; enumerate `/sites/{id}/drives`.
- Returns latest state, not history; items can repeat (take the last); no paths (folder renames don't emit children), so **track by item ID**.
- Deletes come back with a `deleted` facet. On SharePoint, cTag is omitted, so use `eTag` + `file.hashes.quickXorHash`
  (the only hash guaranteed on SharePoint; we must implement it).
- A stale token gets `410 Gone` (resync) → full re-enumeration (expensive on big libraries). `?token=latest` skips the initial crawl.
- `deltashowremovedifnull` is **not documented**; don't rely on it. Permission changes need
  `deltashowsharingchanges` + `Sites.FullControl.All`.

**Throttling** ([SPO throttling](https://learn.microsoft.com/en-us/sharepoint/dev/general-development/how-to-avoid-getting-throttled-or-blocked-in-sharepoint-online))
- Delta with a token = 1 RU; uncached delta, list, or upload = 2; permissions = 5.
- Per app per tenant (smallest tier): 1,250 RU/min, 1.2M RU/day. Per user: 3,000 req / 5 min.
- Honor `Retry-After` on 429/503 globally. SPO does **not** send RateLimit headers. Set User-Agent `ISV|Org|App/ver`.
- Budget: 45s polling ≈ 1.3 RU/min per library per user. The daily budget in small tenants caps users × libraries, which is why push matters.

**Push**
- **socket.io** ([docs](https://learn.microsoft.com/en-us/graph/api/subscriptions-socketio?view=graph-rest-1.0)): GA in v1.0, `GET /drives/{id}/root/subscriptions/socketIo`,
  delegated only (`Files.Read`+), outbound websocket so it works on desktops. It's a signal only, so run delta on each notification. No
  documented latency or lifetime.
- Webhooks: need a public HTTPS endpoint or Event Hubs. Documented average is <1 min, **max up to 6 hours**. Only worth it for a
  future headless/app-only mode via a relay.
- The real OneDrive client uses WNS/APNs, which aren't available to third parties.

**Transfers**: simple PUT ≤250 MB. Otherwise use upload sessions (chunks multiples of 320 KiB, 5–10 MiB recommended,
resumable via `nextExpectedRanges`). Pass `@microsoft.graph.conflictBehavior` in the URL. Use `If-Match: <eTag>` for optimistic
concurrency (412 on mismatch). `downloadUrl` needs no auth and lasts ~1h, so don't cache it.

**Auth**: public client, auth-code + PKCE (loopback) with device-code fallback; delegated keeps per-user permissions
and is required for socket.io. Admin-scoped option: `Sites.Selected` / `Lists.SelectedOperations.Selected` /
`Files.SelectedOperations.Selected` (delegated supported) + a per-site `POST /sites/{id}/permissions` grant ([overview](https://learn.microsoft.com/en-us/graph/permissions-selected-overview)).
Support bring-your-own app registration; optionally publish a verified multi-tenant client ID.

## 3. QA environment

- **M365 Developer sandbox** ([FAQ](https://learn.microsoft.com/en-us/office/developer-program/microsoft-365-developer-program-faq)): free E5, 25 users, includes Entra P2/Intune/Purview,
  so we could test Conditional Access and labels. Only for VS Pro/Enterprise subscribers, eligible partners (incl. Action Pack), or
  Premier/Unified support customers. 90-day renewal; may need to be recreated, so script everything.
- **Personal MSA / bare Azure subscription**: comes with an Entra tenant but **no SharePoint**. Licensed users must be work accounts.
- **Cheapest paid**: SharePoint Plan 1 standalone stopped selling 2026-05-31. M365 **Business Basic** (~$6–7/user/mo, 30-day
  trial, card required). Business Premium (~$22) adds Entra P1/Intune for enterprise-control testing.
- **jamkazam.com tenant**: production, so only use a dedicated private site + `Sites.Selected` grant. No destructive or policy testing.
- **Custom domain** (sethcall.com): optional; `*.onmicrosoft.com` is fine.
- **Fixture scripting**: CLI for Microsoft 365 (`m365 setup` registers its own app) or PnP PowerShell (you must register your own app since
  2024-09) or Graph (`POST /sites` is beta-only). Prefer one long-lived site with **a fresh library per test run**: deleted
  sites sit in the recycle bin for 93 days.

## 4. Enterprise controls (roadmap)

What the official client does and what we must do ourselves:

| Control | Official OneDrive | Us |
|---|---|---|
| Cloud delete | Since 2026-05, removes the local copy (no local recycle bin) | Implement; offer local quarantine + mass-delete threshold |
| Access revoked | Not documented | Configurable delete-on-revoke |
| Retention labels | Deleting a locked record fails and the file reappears | Handle the failed delete, restore, explain |
| IRM / sensitivity labels | Syncs encrypted bytes | Pass through unchanged; never strip protection |
| Domain-join / file-type restrictions | `Set-SPOTenantSyncClientRestriction` (**OneDrive client only**) | Not enforced on us; mirror in our policy file |
| Admin policy (Group Policy / plist) | Yes | Implement the same model |
| Conditional Access | Enforced | **Enforced on us automatically**; device compliance needs WAM (Win) / Enterprise SSO (Mac) |
| Intune selective wipe | Edge only on Windows | Wipe when the token is revoked |
| Audit | Purview sync events | Our calls appear under our app ID; add a local JSONL audit log |

**Priority:** (1) minimal scopes + `Sites.Selected` + verified client ID → (2) safe deletes (quarantine, threshold,
retention failures) → (3) admin policy file / MDM → (4) device-compliance passthrough → (5) delete-on-revoke / wipe →
(6) audit log export → (7) encryption-at-rest checks → (8) label/IRM display.

## Open risks

1. No Microsoft latency SLA for delta or socket.io. Office autosave adds its own lag.
2. Per-tenant RU budget limits users × libraries at fast poll rates.
3. Admin consent needed in most tenants (adoption friction).
4. `410 Gone` resync cost on large libraries.
5. A socket.io v4 client in Rust against Graph is unproven.
6. Placeholder files (CfAPI / File Provider) are a large, separate effort. **Not needed for the spike.**
