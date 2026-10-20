# Design: managing who can access a folder or file

Status: **built** (`access show|set|check|inherit`, `web`, and the sync fix in §6). Decisions in §9. Still to do:
groups (§8). The lockout test with a second user is done (§7), and led to the fixes in §6. User-facing docs: `docs/user-guide.md` §8.

## 1. Goal

From the CLI, and through the library, a site owner can:

- **Set** exactly who can read and who can write a folder or file. The given list replaces what it inherited from the
  site; nobody else gets in, apart from the site's owners (§4.2).
- **Show** who has access to a folder or file.
- **Check** whether a given person can read or write it.
- **Get to the right SharePoint page** for anything Graph can't do, or does better in the browser (§5). SharePoint's
  own navigation is hard to find your way through; a direct link is useful in itself.

This uses the permissions already requested (`Files.ReadWrite.All`, delegated). No new consent is needed.

## 2. What the probe proved (QA tenant, 2026-10-12)

The probe used the `raw` command in `tools/fixtures`, against `runs/perm-probe/` in the QA site.

| Step | Result |
|---|---|
| List a folder's permissions | Shows the site's three groups (Owners: owner, Members: write, Visitors: read). Graph doesn't mark them as inherited |
| Share the folder with a specific person (`invite`, read, no email), **as a site member** | Works |
| Remove the Visitors group from the folder, **as a site member** | **403**. Removing access needs a site owner |
| The same, **as a site owner** | Works, once SharePoint had picked up the new owner (about 7 min after the Entra change) |
| Remove Members too | Works. The folder now has only Owners plus the named person. A file inside follows the folder; the parent folder is unchanged |
| A member who isn't on the list reads the folder | **"Not found"**: SharePoint hides it entirely (seen about 7 min after the role change; access changes themselves are immediate) |
| A member given write on another restricted folder uploads into it | Works |

**Consequences:**
- **Set and remove need a site owner.** Granting alone works for members.
- **Removing what a folder inherited is two documented calls,** no special API:
  1. Sharing it with someone (`invite`) gives the folder its own permissions, starting as a copy of the inherited ones.
  2. Deleting each permission that shouldn't stay then works, because the copies are now the folder's own.
- **Restoring inheritance isn't possible through Graph.** `access inherit` links to SharePoint's page for it (§4.4).

## 3. Graph calls (all documented, v1.0)

| Purpose | Call | Docs |
|---|---|---|
| List | `GET /drives/{d}/items/{id}/permissions` | [driveItem: list permissions](https://learn.microsoft.com/graph/api/driveitem-list-permissions) |
| Grant | `POST /drives/{d}/items/{id}/invite` with `requireSignIn: true`, `sendInvitation: false` | [driveItem: invite](https://learn.microsoft.com/graph/api/driveitem-invite) |
| Change a grant's role | `PATCH /drives/{d}/items/{id}/permissions/{pid}` | [permission: update](https://learn.microsoft.com/graph/api/permission-update) |
| Remove | `DELETE /drives/{d}/items/{id}/permissions/{pid}` | [permission: delete](https://learn.microsoft.com/graph/api/permission-delete) |
| Organisation's domains | `GET /organization?$select=verifiedDomains` | [organization: get](https://learn.microsoft.com/graph/api/organization-get) |
| Links into SharePoint | `GET /drives/{d}/items/{id}?$select=webUrl,sharepointIds` | [sharepointIds](https://learn.microsoft.com/graph/api/resources/sharepointids) |
| Can I write? | `createUploadSession`, then cancel it (nothing is written until a session finishes) | [upload session](https://learn.microsoft.com/graph/api/driveitem-createuploadsession) |

Permission calls cost about 5 RU each, so these commands are for occasional admin use, not sync. The request
accounting counts them as their own kind (`permissions`, 5 RU).

## 4. Behaviour

### 4.1 `access show <path>`

One line per permission: the role, and who (a person, an Entra or Microsoft 365 group, a SharePoint site group with
a link to its members page, or a sharing link with its scope). Then whether the item has its own permissions,
decided by comparing with the parent's list (Graph doesn't say), and a link to its SharePoint permissions page.

### 4.2 `access set <path> --read <people> --write <people>`

**The result:** exactly the listed people with the listed roles, plus the **site Owners group**.
- **Owners always stay,** so nobody can lock the site's own administrators out of a folder. That's any site group or
  group with the owner role: in a group-connected site, the site's Owners group, and also the Microsoft 365 group's
  owners, which SharePoint lists once the site has more than one owner and won't remove.
- `<people>`: comma-separated email addresses or user principal names. A person on both lists gets write.
- `--owners-only` instead of the lists: only the site owners.

**Before changing anything:**
- Every person must be in one of the organisation's verified domains, unless `--allow-external`. A typo then fails
  instead of inviting a stranger.
- The library root is refused unless `--allow-root`.
- The plan is printed (grant, change, remove, keep) and confirmed, unless `--yes`. `--dry-run` stops there. Not on a
  terminal and no `--yes`: refused.

**Order,** so a failure partway never removes more access than intended:
1. **Grant** every listed person (`invite`, no email unless `--notify`). Granting everyone, not only those missing,
   makes sure the item gets its own permissions. With `--owners-only`, the signed-in user is granted read for this
   step and removed again in step 3.
2. **Read the permissions again** and plan against them. If a listed person isn't there (an email alias that isn't
   their sign-in name, for example), stop: nothing has been removed.
3. **Change roles, then remove** everything not on the list and not Owners: site groups, other people, other groups,
   sharing links. A refusal stops here. If the first removal is refused, the operator isn't a site owner: "removing
   access needs a site owner… Access was added, nothing was removed".
4. **Read back** and print the result.

### 4.3 `access check <person> <path>`

| Answer | When |
|---|---|
| **read** / **read and write** | The person has a direct grant, or a sharing link names them |
| **possibly**, naming the groups | A group or site group is granted, and the person may be in it |
| **no access** | No direct grant and no groups (site administrators can still open it) |

- **Groups are only "possibly"** because reading another person's group memberships needs a directory permission
  (`GroupMember.Read.All`) that we don't request. The answer links to the item's permissions page, whose **Check
  Permissions** button answers it exactly.
- **`check me`** is exact: reading the item proves read; write is tested with an upload session that's cancelled
  before anything is sent.
- Exit code 0 when access is certain, 1 otherwise.

### 4.4 `access inherit <path>`

Graph can't make an item inherit again, so this opens the item's SharePoint permissions page and says what to
click: "Delete unique permissions".

### 4.5 Paths

`<path>` is either a local path inside a synced library folder (found in the sync state, so locally renamed names
map back exactly; otherwise the names are mapped back), or `--library <name> <path in library>`. Folders and files
both work. A folder's setting applies to everything inside it that doesn't have its own.

## 5. Links into SharePoint

`looper-sharepoint web <path> [page]` opens (or `--print`s) a page for the item, built from its `webUrl` and
`sharepointIds`:

| Page | URL |
|---|---|
| `item` | the item's `webUrl` |
| `permissions` | `{site}/_layouts/15/user.aspx?obj={list},{item},LISTITEM&List={list}` (library root: `obj={list},doclib`). Inheritance, groups, Check Permissions |
| `versions` | `{site}/_layouts/15/Versions.aspx?list={list}&ID={item}` (files) |
| `recycle-bin` | `{site}/_layouts/15/RecycleBin.aspx` |
| `site-permissions` | `{site}/_layouts/15/user.aspx` |
| `library-settings` | `{site}/_layouts/15/listedit.aspx?List={list}` |
| `site` | the site |
| (site group members, in `access show`) | `{site}/_layouts/15/people.aspx?MembershipGroupId={principal id}` |

**Commands that promise a feature but are links:** `access inherit` is one. More may follow the same pattern where
Graph has no API, e.g. granting a SharePoint site group (§8). Each says plainly that it opens SharePoint and what to
click.

## 6. Interaction with sync

**The risk:** to a person who loses access, their folder looks deleted (delta reports it gone, as for a delete).
Before this fix, two-way sync would then upload their local edits again, and anything new they'd put in the folder
would recreate the folder in SharePoint, outside the restriction.

**The rule now** (two-way sync):
- A file deleted in SharePoint **whose folder is still there** and that has local edits: uploaded again, as before.
- A folder that disappears: its unedited files are deleted locally as before. Anything in it with local edits, and
  anything added locally, is **moved to `<root>/not-uploaded/<library>/<date time>/<path>`**, reported as needing
  attention, and never uploaded. The local folder is then removed, so it isn't recreated.
- `not-uploaded` is reserved: no library may use it as its `local` name.

**Found in the live test with a second user** (2026-10-13):
- **Delta doesn't report access changes** for the person affected: neither losing access (nothing in 9 minutes of
  polling) nor getting it back. The headers that would (`Prefer: deltashowsharingchanges`, …) need
  `Sites.FullControl.All`, which we won't request. **Fix:** each library is listed in full once a day
  (`FULL_SYNC_EVERY`); a full listing treats what it no longer sees as gone, so the rule above applies then, and
  what's newly visible downloads. Until then, an edit in a folder you've lost fails with "not found" and is retried;
  nothing is recreated.
- **Read-only access:** an upload refused with 403 used to be an error retried every run, which also stopped the delta
  link being saved. **Fix:** the local copy goes to `not-uploaded/` and SharePoint's version is downloaded back.
  A 403 isn't always lasting (once, among 14 parallel uploads into a new folder, as an owner), so it's set aside only
  if a write check on the file (or, for a new file, its folder) also fails; otherwise it's an error retried next run.
  A refused name claim (below) is likewise asked twice.
- **Guests** are listed with no email, only their sign-in name `name_domain#ext#@<tenant domain>`; `set` and
  `check` now match that to their email address.
- `check me` on something you can't see answers "no access, or it doesn't exist" (exit 1) instead of an error.

**Restricting a single file** (tested 2026-10-13). To someone who loses access to one file but can still write its
folder, the file looks deleted, and "deleted in SharePoint, edited locally" uploads their copy again. Seen live:
SharePoint accepted that upload as `a (1).txt` beside the hidden file, readable by everyone with access to the folder.
**Fix:** a re-upload first claims the name with an **empty** file. A real delete frees the name, so the claim gets it
and the content goes into that item. A hidden file keeps the name: the claim comes back renamed (upload sessions) or
refused with 403 (simple upload; seen live), so the empty file, if any, is deleted and the local copy is set aside
("its name is still taken: your access to it was probably removed"). Nothing with content is ever written. The
pending re-uploads are kept in the sync state (`meta.reuploads`), so a failed attempt is retried the same way, never
as a plain new upload (seen live: that also leaked).

**Tested:** the live test `vanished_folder_sets_local_changes_aside` deletes a folder with local edits and new
files. The loss-of-access cases (folder, single file, read-only) were run by hand with the second user (§7).

## 7. Testing

- **Unit tests:** `plan` (Owners kept; site groups and links removed; role changes; a person on both lists; nothing
  to do; owners only), `check`, parsing Graph's permission JSON, the SharePoint links, and the set-aside decision.
- **Live, done:**
  - As a member: `set` grants, then stops at the first removal ("Access was added, nothing was removed").
  - As an owner: `set` leaves exactly the owners plus the listed people; a role change (read → write) updates in
    place; `--owners-only` leaves only the owners; `show`, `check` and `check me` read it back.
  - The §6 sync test.
- **QA setup:** `spqa1` is now a permanent owner of the QA site's group (2026-10-12).
- **Second user:** `spqa2@sethcall.com` created and added to the QA group as a member (2026-10-13). It needs a licence
  before it can open SharePoint.
- **Second user, done (2026-10-13)**, with the guest `sethcall@gmail.com` in its own profile (`local/guest/`,
  gitignored), as a member of the QA group: locked out → "not found" at once; local edits and new files set aside on
  losing access; read access with `--allow-external` → `check me` says read only; read-only edits set aside and
  SharePoint's version restored; access regained → files back at the daily full listing.
- **Not automated:** the guest is a personal Microsoft account, which can't use the password grant the live tests
  use. `spqa2@sethcall.com` exists (QA group member) for that, but has no licence (the trial's 2 seats are used).

## 8. Next: groups

Designed in [access-groups-design.md](access-groups-design.md) (draft for review).

## 9. Decisions

| # | Question | Decided |
|---|---|---|
| 1 | Always keep the site Owners group | Yes, to avoid lockout |
| 2 | Email the people granted access | No; `--notify` to opt in |
| 3 | Groups in `--read` / `--write` | People first; groups next (§8). Groups are important |
| 4 | QA setup for the live test | `spqa1` permanent site owner; the guest `sethcall@gmail.com` as the second user (licence not bought) |
| 5 | Fix §6 before shipping `access set` | Yes, done |
| 6 | Link to SharePoint where the UI is needed | Yes: `web`, links in `show` and `check`, and link-only commands like `access inherit` |
