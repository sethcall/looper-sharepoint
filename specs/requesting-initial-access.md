# Requesting initial access

Status: **built** (2026-10-12): §8's code changes, and the kit in `docs/access-request/` (email, attachment, admin
script). §10 lists the corrections to the original draft.

**Organisation-neutral.** This spec, like the repository, names no real organisation. *Contoso* stands for the
organisation asked, and *the requester* for the person asking. The real values (organisation, app name, requester,
licence count) live only in the gitignored `local/access-request.env`.

## 1. Goal and deliverables

Ask the organisation's security lead, Entra admin and the requester's manager to approve one Entra application
(here **Contoso SharePoint Sync**; `APP_NAME` in the kit),
so that this tool (CLI, tray app and later apps built on the same library) can sync SharePoint for a pilot group.

| Deliverable | Source (tracked) | Output (not tracked) |
|---|---|---|
| Email body | `docs/access-request/email.md` | pasted into the email |
| Attachment | `docs/access-request/attachment.adoc` (AsciiDoc) | `dist/<app name>-access-request.docx` via `just access-request` |
| Admin setup script | `docs/access-request/create-registration.sh` (Azure CLI), also printed in the attachment's appendix | run by the admin |

The documents go under `docs/` because they describe the tool as it exists. Everything they claim must already be
true when they're sent, so §8's code changes come first.

**Building the .docx:**
- `asciidoctor -b docbook5`, then `pandoc -f docbook -t docx`, filled from `local/access-request.env`. Word
  styles are pandoc's defaults; a styled `--reference-doc` could be added later, kept outside the repository.
- asciidoctor is installed. pandoc isn't: `brew install pandoc`.
- The `.docx` is generated, never edited by hand.

## 2. The app's name

**How the name works:**
- The name Microsoft shows on the consent and "approval required" screens, in the Entra admin center and in sign-in
  logs is the **app registration's display name**. The client can't set it.
- What the client controls through configuration is **which** registration it signs in with: `account.tenant` and
  `account.client_id`.
- So naming the app means creating the registration with that name, then pointing the config at it.

### 2.1 Creating the registration: the admin does it

**The organisation may block non-admins from the Entra admin center** (it did in the first case, 2026-10-12: "You
don't have access"). Then the requester can't register the app themselves.
- **No workaround.** Command-line registration might still be possible, but it would sidestep a control the organisation set on
  purpose, just before asking the same admins for approval.
- **The admin creates the registration and grants consent in one sitting.** For a security review this is the
  stronger arrangement anyway: the organisation creates and owns the app from the start, rather than approving something an
  employee made.

**What the request gives the admin** (the attachment's appendix A, from [docs/admin-setup.md](../docs/admin-setup.md)
with this name):

| Setting | Value |
|---|---|
| Name | the app name, e.g. `Contoso SharePoint Sync` |
| Supported account types | Accounts in this organizational directory only (single tenant) |
| Redirect URI | Public client/native (mobile & desktop): `http://localhost` |
| Allow public client flows | Yes (device-code sign-in). No, if they'd rather allow browser sign-in only |
| API permissions (delegated, Microsoft Graph) | `Files.ReadWrite.All`, `Sites.Read.All`, `offline_access`, `User.Read`, then **Grant admin consent** |
| Secrets and certificates | None |
| Recommended | Enterprise applications → the app → Properties → **Assignment required = Yes**, and assign the pilot group |
| Optional | `Sites.Selected` instead of the two `.All` permissions, granted per site (§4.2) |

- **The same settings as a script:** an Azure CLI block (`docs/access-request/create-registration.sh`, also printed
  in the appendix). The admin can paste it instead of clicking through.
- **What the admin sends back:** the **Directory (tenant) ID** and the **Application (client) ID**. They aren't
  secrets.

**No screenshot.** The approval screen only exists once a registration does, so there is nothing to capture
before the admin acts. The settings table takes its place.

**After approval:**
1. The requester installs the signed app on their work machine.
2. They write `config.toml` with the two IDs, `app_name` and a library.
3. They sign in. Consent is already granted, so it goes straight through.

**Not used:** a multi-tenant registration in the requester's own tenant. It would show "unverified publisher", and
the app would be owned outside the organisation.

### 2.2 Our own screens use the same name

**No company settings in the repository.** The repository stays free of company context: an organisation's tenant,
client ID and name never go into it.
- Every user needs a config file before the tool does anything. There are no built-in defaults for the tenant or
  client ID.
- **Without a file:** the CLI refuses to run and says where to create one. The tray shows "Not set up yet" and does
  nothing else.
- **A file still holding `config.example.toml`'s placeholder tenant or client ID** is refused the same way.
- So an organisation's first users are told the values, and write the file themselves. That's fine for a pilot.

The client shows a configurable product name, set in the config file's `[account]` section (and later by the
machine policy layer, [specs/policy.md](policy.md)).

```toml
[account]
tenant    = "<tenant id>"
client_id = "<the registration's client id>"
app_name  = "Contoso SharePoint Sync"   # default "looper-sharepoint"
publisher = "Contoso"                   # default "looper-sharepoint"
```

`app_name` appears in:
- the browser page after sign-in ("Signed in to Contoso SharePoint Sync");
- `login` and `whoami` output;
- the tray's menu title, tooltip and status window title;
- the User-Agent on every request, `ISV|Contoso|Contoso SharePoint Sync/<version>`. Microsoft asks for this format
  to identify an app's traffic.

- **The User-Agent** drops characters a header can't carry, so the name above is sent as `ContosoSharePointSync`.
- The macOS app bundle keeps its name (Looper SharePoint). An organisation-branded build isn't needed for this request.

## 3. One approval for every app built on the library

- **One registration, many programs.** An Entra application is identified by its client ID. Every program built on
  the library and configured with the same `client_id` *is* the same Entra application. That covers the CLI, the
  tray, and any separate app embedding the engine.
  - **One consent covers all of them.** Users of a new app aren't asked again, and the admin isn't either.
  - **Admin controls apply to all of them:** user assignment, Conditional Access, disabling the app and revoking
    consent.
  - **Sign-in logs** show them all under one name.
- **The permissions are fixed by the library.** The scopes requested are a constant in the library's auth module.
  A program that needs different permissions requires a new admin consent, which is the control working as
  intended.
- **They share one request budget.** SharePoint's limits are per app registration per tenant, so all programs
  together count against it. §5 already budgets per user, not per program.
- **Each program signs the user in separately** unless it shares the token store.
  - Tokens are kept in the macOS keychain under the service `looper-sharepoint`.
  - A different program, signed by a different developer, reading that item gets a macOS keychain prompt.
  - Signing in again is also fine: no new consent is needed.
- **Rule for programs built on the library:** take `account.client_id` from configuration and use the library's
  sign-in. Never embed a different client ID.

The email says this in one sentence. The attachment has the detail (§9, section 8).

## 4. Security facts for the attachment

Checked on 2026-10-11 and 2026-10-12.

### 4.1 Network and transport

- **Microsoft hosts only.** The client talks to four, all over HTTPS or WSS:

  | Host | Purpose |
  |---|---|
  | `login.microsoftonline.com` | Sign-in and token refresh |
  | `graph.microsoft.com` | The Microsoft Graph API |
  | `<tenant>.sharepoint.com` | File content (downloads and upload sessions) |
  | `*.pushnp.svc.ms` | Microsoft's change-notification channel, a websocket the client opens outward |

  There's no other service, no telemetry and no inbound connection.
- **TLS:**
  - TLS is provided by rustls, a memory-safe Rust implementation. OpenSSL isn't used, and rustls supports only
    TLS 1.2 and 1.3.
  - All four hosts negotiate **TLS 1.3** (`TLS_AES_256_GCM_SHA384`), verified 2026-10-11.
  - **TLS 1.3 is required by default** for every connection, websocket included. `network.min_tls = "1.2"` exists
    only for networks whose TLS-inspecting proxy can't do 1.3. The admin will be able to lock it once the policy
    layer exists.
  - **Certificates are verified against the operating system's trust store** for every connection, so a corporate
    root CA behaves the same everywhere.
  - Verified live on 2026-10-12 against the QA tenant: sign-in, Graph, a sync and the change-notification socket,
    all with TLS 1.3 required.

### 4.2 A standard, documented, Microsoft-supported integration

**Everything the tool does is standard.** It uses only Microsoft's public, documented interfaces, in the way
Microsoft's developer documentation describes. Microsoft's guidance laid out the whole integration; the tool follows
it rather than inventing anything:
- **Sign-in:** the OAuth 2.0 flows of the Microsoft identity platform.
  - [Authorization code flow with PKCE](https://learn.microsoft.com/entra/identity-platform/v2-oauth2-auth-code-flow)
  - [Device code flow](https://learn.microsoft.com/entra/identity-platform/v2-oauth2-device-code)
  - [Public client (desktop) applications](https://learn.microsoft.com/entra/identity-platform/msal-client-applications)
- **API:** Microsoft Graph **v1.0**, the generally available version. No beta endpoints, no undocumented SharePoint
  endpoints, no screen-scraping.
- **The sync pattern itself:** [Best practices for discovering files and detecting changes at
  scale](https://learn.microsoft.com/onedrive/developer/rest-api/concepts/scan-guidance?view=odsp-graph-online).
  It tells a sync client to:
  1. track changes with [delta](https://learn.microsoft.com/graph/api/driveitem-delta);
  2. be notified of changes instead of polling (this tool uses the documented [socket.io
     endpoint](https://learn.microsoft.com/graph/api/subscriptions-socketio));
  3. download and upload through the documented driveItem
     [content](https://learn.microsoft.com/graph/api/driveitem-get-content) and [upload
     session](https://learn.microsoft.com/graph/api/driveitem-createuploadsession) APIs.
- **Load:** Microsoft's throttling guidance (§5).

### 4.2.1 Microsoft Graph calls

| Purpose | Graph calls |
|---|---|
| Find the configured site and library | `GET /sites/{host}:{path}`, `GET /sites/{id}/drives` |
| Track changes | `GET /drives/{id}/root/delta` (change tracking with a token) |
| Download and upload files | driveItem `content`, `createUploadSession` (conditional on the item's eTag) |
| Create, rename and move | `POST …/children` (folders), `PATCH /items/{id}` |
| Delete | `DELETE /items/{id}`, which sends the item to the **site recycle bin**. `permanentDelete` is never called |
| Live updates | `GET /drives/{id}/root/subscriptions/socketIo` |
| Who is signed in | `GET /me` |

No mail, Teams, user-directory, admin or permission APIs are called.

**Delegated permissions:**

| Permission | Why |
|---|---|
| `Files.ReadWrite.All` | Read and write files in libraries the user can already access |
| `Sites.Read.All` | Find sites and libraries by URL |
| `offline_access` | Stay signed in without repeated prompts |
| `User.Read`, `openid`, `profile` | Identify the user |

- **Delegated means the tool acts as the signed-in user.** It can never see or change anything the user couldn't
  in the browser; SharePoint's permissions decide.
- **Least-privilege option for the admin:** delegated `Sites.Selected` instead of the two `.All` scopes. The admin
  then grants the app access per site, and nothing else is reachable even for users with wider rights. This is
  documented in the appendix.

### 4.3 Sign-in and tokens

- **Public client, no secret.** The tool has no client secret or certificate to leak.
- **Interactive sign-in** uses the authorization code flow with PKCE, in the system browser.
  - The organisation's MFA, Conditional Access and SSO apply unchanged.
  - The redirect goes to a listener on the loopback address (`127.0.0.1` / `::1`) on a random port. It exists only
    during sign-in, checks the `state` value, and accepts one response.
- **Device-code sign-in** is available for machines without a browser. The admin can disable it by turning off
  "Allow public client flows".
- **Token storage:**
  - The refresh token is kept in the **macOS keychain**.
  - Access tokens are kept in memory only.
  - Nothing is written to files or logs.
- **Revocation:** the admin can revoke a user's sessions, remove a user's assignment, revoke consent or disable the
  app. Each stops the tool at its next request.

### 4.4 On the user's machine

- The local control socket, which the CLI uses to talk to the running app, is restricted to the owning user (mode
  `0600`).
- Sync state lives in the user's Library folder, never inside the synced folder.
- **Logs:** they record paths and item IDs, never file contents or tokens.
- **Code signing:** the macOS app is signed with an Apple Developer ID and notarized by Apple.

### 4.5 Respecting SharePoint as the source of truth

- **Deletes:**
  - Deleted in SharePoint → deleted locally by default (`on_remote_delete = "delete"`), or kept (`"keep"`).
  - Anything the tool deletes in SharePoint goes to the recycle bin, recoverable for 93 days.
- **Overwrites:**
  - Edits upload in place, so **version history** is kept.
  - An upload only succeeds if SharePoint still has the version the tool last saw.
  - When both sides changed, both versions are kept.
- **Mass deletes:** a sync that would delete more than 25% of a library (10 files or more), in either direction, is
  refused until someone explicitly allows it.
- **Read-only options:** `direction = "download_only"`, or `read_only` per library, never upload.

### 4.6 Admin controls

- **Who can use it:** the admin can require assignment ("Assignment required"), so only a pilot group can sign in.
- **Conditional Access:** policies can target the app by its client ID.
- **Activity:** everything the tool does appears in the SharePoint audit log as the user, through this app.
- **Planned:** a machine policy layer that locks settings, such as forcing `download_only` or limiting which sites
  may sync. Described in [specs/policy.md](policy.md); the documents describe it as planned, not existing.

## 5. Load on SharePoint (resource units)

The original draft said "RSU". Microsoft's term is **resource units (RU)**: each request costs 1–5 RU. Limits are per app
registration, per tenant.

**Example limits:** for a tenant of 20,000 licences (the kit's `LICENSES`), in the 15,001–50,000 tier.

| Scope | Window | Limit at that tier |
|---|---|---|
| This app, whole tenant | 24 h | **4,800,000 RU** |
| This app, whole tenant | 1 min | **5,000 RU** |
| All apps, whole tenant | 5 min | 75,000 RU |

**Why 100 users is a noticeable share of a 20,000-license tenant's budget:**
- **The budget is per app, not per user.** It's one pot for this app across the whole tenant, and it grows slowly
  with size: 20× the licenses of a small tenant gets only 4× the budget. Microsoft sizes it for one app doing
  background work for an organisation, not for a fraction of every user's desktop.
- **Microsoft's own OneDrive client doesn't draw from it.** Every third-party sync client does.
- **The tool isn't chatty.** A typical user costs about one request a minute (1,400 RU over a day).
  - Of that, about 60% is fixed cost. Per library, a live connection needs about 100 requests a day: a safety check
    every 15 minutes, plus subscription renewals and the check after each renewal.
  - The rest is about 1.5 RU per file changed per device, which any sync tool pays to download a change.
- **Capacity:** at that rate, the daily budget covers about **3,400 users**.
  - **Possible later:** a safety check every hour instead of every 15 minutes, and skipping the check after a
    seamless renewal. That would roughly halve the fixed cost, to about 6,000 users.
  - **Not a fit for all 20,000 users:** that isn't what this tool is for. OneDrive remains the company-wide client.

**Per-user cost:** measured on our QA tenant (see
[docs/sharepoint/throttling.md](../docs/sharepoint/throttling.md#measured-cost)).
- Idle library with live updates: 190 RU/day.
- Each change downloaded: 1.5 RU.
- Each change uploaded: 4.8 RU.
- Polling, when live updates are blocked: 1,870 RU/day per library.

**100 users, each syncing 3 libraries:**

| Scenario | Per user per day | 100 users per day | Share of the daily limit |
|---|---|---|---|
| **Typical:** live; 100 notification batches per library; 200 changes down and 50 up | ≈ 1,400 RU | ≈ 141,000 RU | **2.9%** |
| **Live updates blocked by the network:** polling every 45 s | ≈ 6,200 RU | ≈ 615,000 RU | 12.8% |
| **Very busy:** every library changing every 30 s, all day | ≈ 9,800 RU | ≈ 975,000 RU | 20.3% |

- **Per minute:** the typical case averages about 100 RU/min, **2%** of the per-minute limit. That's under 1% of the
  tenant-wide 5-minute limit shared with every other app.
- **The one heavy moment is a user's first sync,** which downloads their whole library.
  - The client paces itself to 8 requests a second, about 500–1,000 RU/min per user.
  - So the rollout plan starts **at most 5 first syncs at a time**.
  - The client also always obeys SharePoint's `Retry-After` responses.
- **The homework, cited in the attachment:**
  - [Avoid getting throttled or blocked in SharePoint Online](https://learn.microsoft.com/en-us/sharepoint/dev/general-development/how-to-avoid-getting-throttled-or-blocked-in-sharepoint-online)
  - [Microsoft Graph throttling limits](https://learn.microsoft.com/graph/throttling-limits)
  - [Best practices for discovering files and detecting changes at scale](https://learn.microsoft.com/onedrive/developer/rest-api/concepts/scan-guidance?view=odsp-graph-online)
  - [SharePoint Service Prioritization](https://learn.microsoft.com/en-us/sharepoint/sharepoint-prioritization)
- **How the client follows that guidance:**
  - change tracking (delta) instead of scanning;
  - push notifications instead of frequent polling;
  - client-side pacing;
  - honouring `Retry-After`;
  - a registered app ID with a decorated User-Agent;
  - never reading permissions.

## 6. Why it was built (email and attachment narrative)

- **The problem with OneDrive's client.**
  - The organisation uses SharePoint as the enterprise source of truth for documents and permissions.
  - OneDrive's sync client can't be told to sync now, in either direction.
  - When it gets stuck, which happens, there's no way to see that or why.
  - That isn't good enough for local-first work across teams, especially for people and AI agents that work on
    plain local files.
- **What this tool adds:**
  - `looper-sharepoint sync` brings a library up to date and waits until it's done, like `git pull`.
  - Status always says whether changes are arriving live or by polling, when the last sync was, and the last
    failure.
  - Changes in SharePoint arrive in about 30 s, and local changes upload in about 3 s.
- **Why SharePoint rather than a deep integration elsewhere.**
  - Nothing else gives per-document and per-folder access control without a deep integration in another system.
    That may still come.
  - This unblocks small teams working agentically and sharing documentation now, while customer data and
    leadership-only material stay segmented by SharePoint permissions.
- **Why not an existing open-source tool.**
  - Tools like **rclone** and the community **onedrive** client for Linux are capable, but they're general-purpose
    Swiss-army knives. rclone alone has dozens of backends and options.
  - They weren't built for enterprise controls: one approved app identity, admin-lockable policy, refusing mass
    deletes, never permanently deleting, and visible sync health.
  - The attachment gives the comparison a short section of its own (§9, section 7).

## 7. Security testing

**Not run for this request** (decided 2026-10-11).
- The documents don't claim scan results.
- They offer source access, so the CRO's team can review or scan it themselves.
- If they ask for scans, the free options are:
  - cargo-audit, cargo-deny and Semgrep CE for the code and dependencies;
  - dynamic tests of the sign-in listener and the control socket;
  - fuzzing the network parsers.
- clippy, which runs on every change, is a lint rather than a security scan, so it isn't presented as one.

## 8. Code changes before sending

1. **Done (2026-10-12): `account.app_name` and `account.publisher`.** They appear on the sign-in page, in `login` and
   `whoami`, in the tray's titles and menu, and in the User-Agent of every request, websocket included.
2. **Done (2026-10-12): TLS 1.3 by default,** with `network.min_tls = "1.2"` as the exception. The websocket uses
   the OS trust store.
3. **Done (2026-10-12): no company settings in the repository.** Without a config file, the tool refuses to start
   (§2.2), and the example file's placeholders are refused too.
4. **Done (2026-10-12): `just access-request`,** which builds the `.docx` (§1).

## 9. Attachment outline (`attachment.adoc`)

1. **Summary:** the request, who it's from, and what the admin is asked to do: create the registration from
   appendix A, grant consent, and send back two IDs.
2. **Why we built it:** §6.
3. **What it does:** a short overview; how two-way sync behaves; live updates.
4. **Access requested:** the app name, permissions and delegated model (§4.2), and the `Sites.Selected` option.
5. **Security:** §4.1–§4.6, opening with §4.2 (standard, documented integration).
6. **Load on SharePoint:** §5, with citations.
7. **Alternatives considered:** OneDrive client, rclone, the onedrive Linux client, deep integration elsewhere.
8. **Other apps built on the same engine:** §3.
9. **Source code:** available on request.
10. **Appendix A, admin steps:** the settings table from §2.1, the Azure CLI script, granting consent, requiring
    assignment, and the `Sites.Selected` option.

**The email body:** about 150 words.
- The ask: create and approve the app (`APP_NAME`). Exact settings are in the attachment: about 10 minutes,
  or one pasted script.
- One-line why.
- One line each on safety, load and source.
- One line saying it uses only Microsoft's standard, documented APIs, as Microsoft's developer documentation lays
  out.
- "Details attached."
- The two IDs to send back.

## 10. Corrections to the original draft

| Draft said | Changed to | Why |
|---|---|---|
| OneLogin | **OneDrive** (the sync client) | The draft describes OneDrive's sync behaviour; OneLogin is a sign-in product |
| rlogin | **rclone** (plus the onedrive Linux client) | rclone is the well-known open-source SharePoint/OneDrive sync tool |
| RSU | **RU** (resource units) | Microsoft's term |
| The requester registers the app and screenshots the approval screen | The admin creates the registration from given settings; no screenshot | §2.1: non-admins are blocked from the Entra admin center |
| The tool sets the app's name | The **registration** sets the name; config selects the registration, and `app_name` names our own screens | Microsoft shows the registration's display name |
| "If we use TLS 1.3" | TLS 1.3 verified on all four hosts, and required by default | §4.1 |
| SAST and DAST | Not run for this request; source offered instead | §7 |
