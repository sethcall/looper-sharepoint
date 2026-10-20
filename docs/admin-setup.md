# looper-sharepoint — Tenant Admin Setup

This is what a Microsoft 365 tenant administrator does so that users can run `looper-sharepoint` against their
SharePoint. It takes about 15 minutes. It's written to hand to an IT admin, and it's the same procedure we used for our QA
tenant (2026-10-11).

`looper-sharepoint` is a **desktop public client**. It signs in **as the user** (delegated permissions), so it can
only ever see what that user can already see in SharePoint. It stores no client secret.

## What the admin needs

- A role that can register apps and grant tenant-wide consent: **Global Administrator**, **Privileged Role
  Administrator**, or **Cloud Application Administrator** + **Application Administrator**.
- A **work account** in the tenant (not a personal Microsoft account). Personal accounts can't sign in to
  admin.microsoft.com ("Login is not supported for consumer users without business presence").
- Users who will sync need a license that includes SharePoint Online (e.g. Microsoft 365 Business Basic/Standard/Premium, E3/E5).

## Step 1: Register the application

**Portal:** [Entra admin center](https://entra.microsoft.com) → Identity → Applications → App registrations → **New registration**

| Setting | Value |
|---|---|
| Name | `looper-sharepoint` |
| Supported account types | *Accounts in this organizational directory only* (single tenant) |
| Redirect URI | Platform **Public client/native (mobile & desktop)**, `http://localhost` |

Then, in the app's **Authentication** page → *Advanced settings* → **Allow public client flows = Yes** (enables device-code sign-in).

Don't create a client secret or certificate. None is needed.

## Step 2: Add API permissions

App → **API permissions** → Add a permission → Microsoft Graph → **Delegated permissions**:

| Permission | Why |
|---|---|
| `Files.ReadWrite.All` | Read/write files in the libraries the user can access |
| `Sites.Read.All` | Discover sites and document libraries by URL |
| `offline_access` | Refresh token, so the user doesn't re-sign-in constantly |
| `User.Read`, `openid`, `profile` | Identify the signed-in user |

All are **delegated**. Access is the intersection of these scopes and the user's own SharePoint permissions.

## Step 3: Grant admin consent

API permissions → **Grant admin consent for \<tenant\>** → Yes. All rows should show a green "Granted" status.

This is required: since 2025, Microsoft's default consent policy blocks users from consenting to `Files.*.All` /
`Sites.*.All` themselves.

## Step 4: Give the developer these values

| Value | Where to find it |
|---|---|
| Tenant ID | App → Overview → *Directory (tenant) ID* |
| Client ID | App → Overview → *Application (client) ID* |
| SharePoint site URL(s) | e.g. `https://contoso.sharepoint.com/sites/Engineering` |

These go into the `[account]` section of `config.toml`. They aren't secrets.

## Optional: tighter, admin-controlled scoping (recommended for production)

To limit `looper-sharepoint` to **specific sites** even if a user has wider access, use *Selected* permissions instead of `.All`:

1. In Step 2, add delegated **`Sites.Selected`** (and/or `Lists.SelectedOperations.Selected`) instead of `Files.ReadWrite.All` / `Sites.Read.All`, then consent.
2. Grant the app access per site (Graph, as an admin):
   ```http
   POST https://graph.microsoft.com/v1.0/sites/{site-id}/permissions
   { "roles": ["write"],
     "grantedTo": { "application": { "id": "<client-id>", "displayName": "looper-sharepoint" } } }
   ```
   or PnP PowerShell: `Grant-PnPAzureADAppSitePermission -AppId <client-id> -DisplayName looper-sharepoint -Site <url> -Permissions Write`.
3. Use `roles: ["read"]` for read-only sites.

Other controls that apply automatically to `looper-sharepoint` like any other app:
- **Conditional Access** (MFA, compliant device, location): target the app by client ID if you want specific policies.
- **Assignment required:** Enterprise applications → looper-sharepoint → Properties → *Assignment required = Yes*, then
  assign only the users/groups allowed to use it.
- **Sign-in and audit logs:** activity appears under the app's name/client ID in Entra sign-in logs and Purview audit.

## Revoking access

- Disable or delete the **Enterprise application** (service principal) to stop all sign-ins immediately.
- Remove a user's assignment (if *Assignment required* is on) to revoke an individual.
- Revoke a user's sessions in Entra to invalidate their refresh tokens.

---

## CLI equivalent (what we ran for the QA tenant)

Using the Azure CLI (`az login` as an admin). The scope IDs are Microsoft Graph's well-known delegated permission IDs.

```bash
GRAPH=00000003-0000-0000-c000-000000000000
APPID=$(az ad app create --display-name sharepoint-sync-dev --sign-in-audience AzureADMyOrg \
  --is-fallback-public-client true --public-client-redirect-uris http://localhost \
  --required-resource-accesses '[{"resourceAppId":"'$GRAPH'","resourceAccess":[
    {"id":"863451e7-0667-486c-a5d6-d135439485f0","type":"Scope"},
    {"id":"205e70e5-aba6-4c52-a976-6d2d46c48043","type":"Scope"},
    {"id":"7427e0e9-2fba-42fe-b0c0-848c9e6a8182","type":"Scope"},
    {"id":"e1fe6dd8-ba31-4d61-89e7-88639da4683d","type":"Scope"},
    {"id":"37f7f235-527c-4136-accd-4a02d197296e","type":"Scope"},
    {"id":"14dad69e-099b-42c9-810b-d002981feec1","type":"Scope"}]}]' --query appId -o tsv)
# (Files.ReadWrite.All, Sites.Read.All, offline_access, User.Read, openid, profile)

SPID=$(az ad sp create --id $APPID --query id -o tsv)
GRAPH_SP=$(az ad sp show --id $GRAPH --query id -o tsv)

# Tenant-wide admin consent (same as the portal button)
az rest --method post --url https://graph.microsoft.com/v1.0/oauth2PermissionGrants --body \
  "{\"clientId\":\"$SPID\",\"consentType\":\"AllPrincipals\",\"resourceId\":\"$GRAPH_SP\",
    \"scope\":\"Files.ReadWrite.All Sites.Read.All offline_access User.Read openid profile\"}"
```

## Appendix: our QA tenant bootstrap (not needed in a real company)

Done once in "Seth Call Dev Tenant" (`sethcallgmail.onmicrosoft.com`); IDs are in `.env`.

1. The tenant's only admin was a personal Microsoft account (sethcall@gmail.com), which **can't use admin.microsoft.com**.
   Created work account `seth@sethcall.com` with Global Administrator via `az ad user create` + a
   `roleManagement/directory/roleAssignments` POST (role template `62e90394-69f5-4237-9190-012177145e10`).
2. As `seth@sethcall.com`, started a **Microsoft 365 Business Basic (no Teams)** trial (2 seats). SharePoint auto-provisioned at
   `https://sethcallgmail.sharepoint.com`.
3. Created test user `spqa1@sethcall.com`; set `usageLocation=US` on both users (required before licensing) and assigned licenses via
   `POST /users/{upn}/assignLicense`.
   - 2026-10-13: added `spqa2@sethcall.com` (member of the QA group) for access-control lockout tests. Its password is in the
     gitignored `local/qa-users.env`. Not licensed yet: the trial's 2 seats are in use.
4. Registered the app + consent (CLI above).
5. Created the QA site by creating a private Microsoft 365 group `sharepoint-sync-qa` (`POST /groups`, `groupTypes: ["Unified"]`,
   owner seth, members seth + spqa1). SharePoint creates the team site automatically:
   `https://sethcallgmail.sharepoint.com/sites/sharepoint-sync-qa`.
6. Verified as `spqa1` through the new app: read site + drive, uploaded `smoke/hello.txt` (quickXorHash returned), delta
   enumeration, and `GET /drives/{id}/root/subscriptions/socketIo` returned a push notification URL.
   - Verification used the password (ROPC) grant for the test account only. That works here because the account has no MFA.
     The app itself uses browser/device-code sign-in, and ROPC won't work in tenants that enforce MFA.
