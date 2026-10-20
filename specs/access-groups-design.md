# Design: groups in `access`

Status: **draft for review** (2026-10-14). Extends [access-control-design.md](access-control-design.md) (people,
built), replacing its §8 outline. Decisions to confirm in §8.

## 1. Goal

`access set` takes groups as well as people, so a folder can be restricted to a team. `show` and `check` say which
groups have access, and how sure the answer is.

There are two kinds of group, and SharePoint treats them differently:

| Kind | Examples | Lives in |
|---|---|---|
| **Directory groups** | A Microsoft 365 group (a team, the site's own group), an Entra security group | Entra ID |
| **Site groups** | The site's Owners, Members and Visitors; any SharePoint group made in the site | The SharePoint site |

## 2. What the probe found (QA tenant, 2026-10-14, as a site owner, on `runs/group-probe`)

| Step | Result |
|---|---|
| `invite` a Microsoft 365 group **by its email** | Works. SharePoint grants the group's **members** (sign-in claim `…federateddirectoryclaimprovider\|<group id>`) |
| `invite` a group **by its directory id** (`recipients: [{ objectId }]`) | Works. This is the only way to name a security group that has no email |
| The group in the permission list | `grantedToV2.group` with display name, email and directory id, plus `siteUser.loginName` (the claim). The site's own group's owners appear the same way, with `_o` on the claim |
| Change a **site group's** role on the item (Visitors read → write) | Works (`PATCH` on its permission), and back again |
| **Add** a site group to an item | No Graph call (as found before). Only SharePoint's "Grant Permissions" page |
| Look up a group by email, list a group's members, read someone else's groups | **403** with this app's permissions |
| Am **I** in these groups? (`POST /me/checkMemberGroups`) | Works |

**Naming trap:** the Microsoft 365 group's members are listed as "sharepoint-sync-qa Members", the same name as the
**site group** "sharepoint-sync-qa Members". They're different grants. `show` must say which kind each one is.

**Not probed:** a security group by id. Creating one in the QA tenant was blocked in this session as a permission
change; it needs your approval, or you can create it yourself (§7). The documentation and the "All Company" result
suggest it behaves the same way.

## 3. `access set` with groups

```
looper-sharepoint access set <path> [--read <people>] [--write <people>]
    [--read-group <groups>] [--write-group <groups>]
    [--read-site-group <names>] [--write-site-group <names>]
    [--owners-only] [--yes] [--dry-run] [--allow-root] [--allow-external]
```

- **`--read-group` / `--write-group`:** directory groups, comma-separated, each **an email address or a directory
  id** (a GUID, as shown in Entra or by `access show`). They're granted with `invite`, exactly like people, in step
  1 of the existing order.
  - **By email:** the domain check applies, as for people.
  - **By id:** there's no domain check; SharePoint refuses an id it doesn't know.
  - A Microsoft 365 group grants its **members**, never its owners.
- **`--read-site-group` / `--write-site-group`:** site groups, by name (case-insensitive), e.g. `"Team Members"`.
  - **Kept, at that role,** if the item still lists the site group. That's always the case the first time a folder is
    restricted, because restricting copies the site's groups. Its role is changed in place if needed.
  - **Not listed** (removed earlier, or never there): `set` **stops before changing anything**. It gives the item's
    permissions page and says: Grant Permissions → type the group's name → choose the level, then run `set` again.
  - This makes the common case one command: `access set Projects/Board --write-site-group "Board Members"` leaves
    only the owners and that site group.
- **The rest is unchanged:**
  - The site's owners always keep access. That covers the Owners site group and the Microsoft 365 group's owners.
  - Everything not listed is removed: people, other directory groups, other site groups and sharing links.
  - A name on both lists gets write.
- **Matching after granting** (the step-2 check): a directory group matches by id or email. If a granted group isn't
  found, `set` stops: nothing has been removed.

## 4. `access show`

Each line names the kind of grant:

```
  owner   site group  sharepoint-sync-qa Owners          (site owners always keep access)
  write   site group  Board Members                      members: <people.aspx link>
  read    Microsoft 365 group  Design team (members)     design@contoso.com · id 1b2c…
  read    security group  Finance readers                id 9a8b…
  owner   Microsoft 365 group  sharepoint-sync-qa (owners)
  read    person      Ann Lee <ann@contoso.com>
```

- Kinds come from the sign-in claim: `c:0o.c|federateddirectoryclaimprovider|<id>` is a Microsoft 365 group's
  members (`_o` for its owners), `c:0t.c|tenant|<id>` is a security group.
- `--json` adds `kind`, `email` and `id` for groups.

## 5. `access check`

- **`check me`:** stays exact for read and write (it tests for real). It also names the directory groups that give
  you access, from `POST /me/checkMemberGroups` with the granted groups' ids.
  - **Site groups can't be checked:** Graph has no call for a site group's members. They stay "possibly", with the
    permissions page's Check Permissions button.
- **`check <someone else>`:** unchanged. A direct grant is certain; groups are "possibly", naming each group and its
  kind, with the Check Permissions link.
  - Making this exact needs a directory permission (§8, decision 2).

## 6. `access add-site-group`: a link where Graph can't

```
looper-sharepoint access add-site-group "<site group>" --role read|write <path>
```

- **Listed already:** sets its role (real, through Graph).
- **Not listed:** opens the item's permissions page and says exactly what to click: Grant Permissions → the group's
  name → the level, with "Share everything in this folder" left as is.
- This is the "commands that promise a feature, but it's a link" pattern from `access inherit`.

## 7. Testing

- **Unit:**
  - Parsing both group kinds and the owners claim from the probe's JSON.
  - `plan` keeps listed groups and site groups, changes roles, removes unlisted ones, and still keeps the owners.
  - A site group that isn't listed stops the plan before anything changes.
  - `show` names the kind of each grant.
- **Live (QA), as the owner:**
  - `set` with `--read-group <QA group email>` and `--write-site-group "sharepoint-sync-qa Members"`.
  - `--read-group <id>`.
  - `check me` naming the group, as the guest (a member of the QA group).
  - A site group that isn't there: refused, nothing changed.
- **Needs you:** a **security group** with the guest in it, to test `--read-group <id>` for a group without email.
  Either approve me creating `looper-qa-readers` in the QA tenant (`az ad group create` and one member add), or
  create it yourself.

## 8. Decisions

| # | Question | Proposed |
|---|---|---|
| 1 | Groups named by email or directory id (no lookup by display name, which needs a directory permission) | Yes |
| 2 | Request a directory permission (`GroupMember.Read.All` and `User.ReadBasic.All`, both admin-consented) so `check <someone>` is exact | **Not now.** It changes the access request that's about to go out. Revisit once the app is approved; "possibly" plus the Check Permissions link covers it until then |
| 3 | Site groups: keep and change role through Graph; adding one is a link (`add-site-group`) | Yes |
| 4 | A site group named in `set` that isn't listed: stop before changing anything | Yes, matching "a failure partway never removes more access than intended" |
| 5 | A Microsoft 365 group grants its members only | Yes (SharePoint's own behaviour) |
| 6 | Security-group test setup in QA | Your call: approve my creating it, or create it yourself |
