# Policy Layers: How Admins Tighten looper-sharepoint

Status: **spec** (not built). As each piece ships, it is documented for admins in `docs/administrate.md`, which describes
only knobs that exist.

## Principle

Out of the box, `looper-sharepoint` is a **developer-friendly** tool. The user's `config.toml` controls everything, and the
defaults never destroy data that exists only locally. Recovery of deleted or overwritten files relies on SharePoint's built-in
recycle bin (93 days, two-stage) and version history; the client keeps no local trash. Strict deployments want deleted
data gone from devices.

An enterprise can optionally **tighten** behavior through policy files that a regular user can't edit. Policy can
set a value, lock it (the user can't override it), or both. Without any policy present, behavior is exactly the user config.

## Layers (highest precedence first)

1. **Machine policy:** a local file protected by OS permissions. Deployed by IT through MDM, GPO, Intune, Jamf, or by hand.
2. **Remote policy:** files in well-known SharePoint locations. The enterprise admin is responsible for locking down their
   SharePoint permissions. The client reads and honors them.
3. **User config:** `config.toml`.
4. **Built-in defaults.**

**Merge rule:** for each key, take the highest layer that sets it. If a layer marks the key `locked`, lower layers can't
change it. If a lower layer tries to, the client uses the locked value and logs a warning; it doesn't fail. Machine policy beats remote
policy so that a device owner (IT) can always override what a site says (decided 2026-10-11).

## 1. Machine policy file

| OS | Location | Must be |
|---|---|---|
| macOS | `/Library/Application Support/looper-sharepoint/policy.toml`, or the managed preferences domain `com.looper-sharepoint` (MDM `.mobileconfig`) | owned by root, not writable by group or other |
| Windows | `%ProgramData%\looper-sharepoint\policy.toml`, or the registry key `HKLM\Software\Policies\looper-sharepoint` (GPO/ADMX, Intune) | ACL: only Administrators/SYSTEM can write |

**Integrity:** the client checks the file's ownership and ACL on every load. If the file is writable by the current
non-admin user, the client **ignores it and logs an error**, because a user-writable policy file could be forged to loosen restrictions.

```toml
# /Library/Application Support/looper-sharepoint/policy.toml
[sync]
local_orphans    = { value = "delete", locked = true }   # mirror strictly: delete anything not on remote
on_remote_delete = { value = "delete", locked = true }
max_delete_fraction = { value = 1.0, locked = true }

[account]
allowed_tenants = { value = ["contoso.onmicrosoft.com"], locked = true }
client_id       = { value = "<company app id>", locked = true }

[remote_policy]
# Where to look for layer 2, and whether it's required.
locations = ["https://contoso.sharepoint.com/sites/it-policy/looper-sharepoint/policy.toml"]
required  = true      # if unreachable or invalid: refuse to sync (fail closed) instead of falling back
```

A plain value (`blocked_extensions = [".exe"]`) means "set but not locked".

## 2. Remote policy files in SharePoint

These are read with the user's own token, so they need no extra permissions.

- **Tenant-wide:** a location named by machine policy (`remote_policy.locations`), e.g. a file in an IT-owned site.
- **Per library:** `/.looper-sharepoint/policy.toml` at the root of a synced library. Site owners can tighten behavior for their own
  library, e.g. Legal sets `read_only = true`.

Remote policy may both **loosen and tighten**, and it overrides the user's `config.toml`.

**Trust problem:** anyone who can edit a policy file can change behavior for everyone who syncs that library. That includes
destructive settings like `local_orphans = "delete"`. Mitigations, in order:
- The client checks its own effective permission on the policy item. If the *current user* can write the policy file, the
  client warns and **ignores the file**. The admin's job is to make policy files read-only for syncing users.
- Per-library policy can never override machine policy, and machine policy can disable per-library policy entirely
  (`remote_policy.per_library = false`).
- Every policy load records the source, item ID, eTag, and last-modified-by in the audit log.

**Refresh:** remote policy is fetched at startup, then whenever delta reports a change to the policy item (it's in the same
library) or on a 15-minute timer for tenant-wide files.

## Planned admin features

- `looper-sharepoint restore`: list and restore items from the site recycle bin and earlier versions (post-spike). Document in
  `docs/administrate.md` when built, as the user-facing recovery path that pairs with strict delete settings.

## Knobs (target list; each moves to `docs/administrate.md` when built)

| Key | Default | Hardened example | Milestone |
|---|---|---|---|
| `account.allowed_tenants` | any | your tenant only | M1 validated, enforced later |
| `account.client_id` | user-chosen | company app registration | later |
| `sync.direction` | `two_way` | `download_only` | M4 |
| `library.read_only` | false | true | M4 |
| `sync.on_remote_delete` | `delete` (recycle bin is the safety net) | `delete`, locked | M1 |
| `sync.local_orphans` | `keep` | `delete` (strict mirror) | M1 |
| `sync.max_delete_fraction` | 0.25 | 1.0 (strict mirror) | M1 |
| `sync.allowed_sites` | any | list of site URLs | later |
| `sync.blocked_extensions` | none | `.exe`, `.dll`, … | later |
| `naming.*` | escape + default map | locked map | M1 (config only) |
| `audit.log` / `audit.required` | off | required, fixed path | later |
| `access.on_revoke` | keep | delete local copies when access is lost | later |
| `access.wipe_on_signout` | false | true | later |

## Limits to be honest about

A plain folder can't stop a user copying files elsewhere. OneDrive's "can't copy out" behavior comes from Intune app
protection / Purview Endpoint DLP on the device, not from the sync client. For that use case, `docs/administrate.md` should
say plainly: pair looper-sharepoint with device DLP, use `download_only` + `read_only`, and enable delete-on-revoke.

## Open questions

1. What format should the MDM plist / registry representation of `{ value, locked }` take? (Likely parallel keys: `X` and `X_Locked`.)
2. What should the default be when `remote_policy.required = false` and the file can't be fetched: use the last cached copy or ignore it? (Leaning toward the last cached copy, with a TTL.)
