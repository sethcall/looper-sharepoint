//! Who can read or write a folder or file: [`list`], [`plan`], [`apply`], [`check`], and [`Links`] into SharePoint's
//! own pages for what Graph can't do. See `specs/access-control-design.md`.
//!
//! Only standard Graph calls on the signed-in user's behalf (`Files.ReadWrite.All`, delegated): SharePoint decides
//! what this user may change. Granting works for site members; removing access needs a site owner.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use looper_sharepoint_core::config::{Config, Library};
use looper_sharepoint_core::naming::local_key;
use looper_sharepoint_core::state::StateDb;
use looper_sharepoint_graph::models::{DriveItem, Permission};
use looper_sharepoint_graph::{GraphClient, GraphError};
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Read,
    Write,
    Owner,
}

impl Role {
    /// The strongest of SharePoint's role names (`read`, `write`, `owner`; `sp.full control` counts as owner).
    fn from_roles(roles: &[String]) -> Option<Role> {
        roles
            .iter()
            .filter_map(|r| match r.to_ascii_lowercase().as_str() {
                "read" => Some(Role::Read),
                "write" => Some(Role::Write),
                "owner" | "sp.full control" => Some(Role::Owner),
                _ => None,
            })
            .max()
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Role::Read => "read",
            Role::Write => "write",
            Role::Owner => "owner",
        }
    }
}

/// Who a permission is for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Who {
    Person {
        name: String,
        email: Option<String>,
        /// User principal name, from SharePoint's login name.
        login: Option<String>,
    },
    /// An Entra ID or Microsoft 365 group.
    Group { name: String },
    /// A SharePoint site group, e.g. "Team Site Members". `id` is its principal id on the site.
    SiteGroup { name: String, id: Option<String> },
    /// A sharing link. `people` are those it was given to, when it names them.
    Link { scope: String, kind: String, people: Vec<String> },
    Other { name: String },
}

impl Who {
    /// Is this the person with this email address or user principal name? A guest has no email here, and signs in
    /// as `name_domain#ext#@<tenant domain>`, which also matches their email address.
    pub fn is_person(&self, who: &str) -> bool {
        match self {
            Who::Person { email, login, .. } => {
                let guest = format!("{}#ext#@", who.replace('@', "_")).to_lowercase();
                [email, login].into_iter().flatten().any(|a| a.eq_ignore_ascii_case(who))
                    || login.as_ref().is_some_and(|l| l.to_lowercase().starts_with(&guest))
            }
            _ => false,
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Who::Person { name, email, login } => match email.as_ref().or(login.as_ref()) {
                Some(a) if !a.eq_ignore_ascii_case(name) => format!("{name} <{a}>"),
                _ => name.clone(),
            },
            Who::Group { name } => format!("group {name}"),
            Who::SiteGroup { name, .. } => format!("site group {name}"),
            Who::Link { scope, kind, people } if people.is_empty() => format!("{kind} link ({scope})"),
            Who::Link { scope, kind, people } => format!("{kind} link ({scope}) for {}", people.join(", ")),
            Who::Other { name } => name.clone(),
        }
    }
}

/// One permission on an item.
#[derive(Debug, Clone, Serialize)]
pub struct Grant {
    /// The permission id (for update and delete).
    pub id: String,
    pub who: Who,
    /// `None` when SharePoint uses a role this tool doesn't know; see `roles`.
    pub role: Option<Role>,
    pub roles: Vec<String>,
}

impl Grant {
    pub fn from_permission(p: &Permission) -> Grant {
        let name = |i: Option<&str>| i.unwrap_or("(unnamed)").to_string();
        let who = if let Some(link) = &p.link {
            Who::Link {
                scope: link.scope.clone().unwrap_or_else(|| "unknown".into()),
                kind: link.kind.clone().unwrap_or_else(|| "sharing".into()),
                people: p
                    .granted_to_identities_v2
                    .iter()
                    .filter_map(|g| {
                        let u = g.user.as_ref()?;
                        u.email.clone().or_else(|| u.display_name.clone())
                    })
                    .collect(),
            }
        } else if let Some(g) = &p.granted_to_v2 {
            if let Some(sg) = &g.site_group {
                Who::SiteGroup { name: name(sg.display_name.as_deref()), id: sg.id.clone() }
            } else if let Some(gr) = &g.group {
                Who::Group { name: name(gr.display_name.as_deref().or(gr.email.as_deref())) }
            } else if g.user.is_some() || g.site_user.is_some() {
                let user = g.user.as_ref();
                let login = g.site_user.as_ref().and_then(|s| s.login_name.as_deref()).and_then(|l| l.rsplit('|').next()).map(str::to_string);
                Who::Person {
                    name: name(user.and_then(|u| u.display_name.as_deref()).or(g.site_user.as_ref().and_then(|s| s.display_name.as_deref()))),
                    email: user.and_then(|u| u.email.clone()),
                    login: login.filter(|l| l.contains('@')),
                }
            } else {
                Who::Other { name: "(unknown)".into() }
            }
        } else {
            Who::Other { name: "(unknown)".into() }
        };
        Grant { id: p.id.clone(), who, role: Role::from_roles(&p.roles), roles: p.roles.clone() }
    }

    /// The site's owners: a site group or group with the owner role. In a group-connected site that's the site's
    /// Owners group and the Microsoft 365 group's owners (SharePoint lists both, and won't remove the second). They
    /// always keep access, so nobody can lock the site's administrators out.
    pub fn is_site_owners(&self) -> bool {
        matches!(self.who, Who::SiteGroup { .. } | Who::Group { .. }) && self.role == Some(Role::Owner)
    }
}

/// Who should have access after `set`: people by email address or user principal name. A person in both lists gets
/// write.
#[derive(Debug, Clone, Default)]
pub struct Desired {
    people: BTreeMap<String, Role>,
}

impl Desired {
    pub fn new(readers: &[String], writers: &[String]) -> Result<Desired> {
        let mut people = BTreeMap::new();
        for (list, role) in [(readers, Role::Read), (writers, Role::Write)] {
            for p in list {
                let p = p.trim().to_lowercase();
                if p.is_empty() {
                    continue;
                }
                if !p.contains('@') {
                    bail!("{p:?} isn't an email address or user name (name@domain)");
                }
                let r = people.entry(p).or_insert(role);
                *r = (*r).max(role);
            }
        }
        Ok(Desired { people })
    }

    pub fn people(&self) -> impl Iterator<Item = (&str, Role)> {
        self.people.iter().map(|(p, r)| (p.as_str(), *r))
    }

    pub fn is_empty(&self) -> bool {
        self.people.is_empty()
    }

    /// People whose domain isn't one of the organisation's (`domains`, lowercase). Granting them would share
    /// outside the organisation, if the tenant allows that at all.
    pub fn outside(&self, domains: &[String]) -> Vec<String> {
        self.people
            .keys()
            .filter(|p| !p.rsplit_once('@').is_some_and(|(_, d)| domains.iter().any(|x| x == d)))
            .cloned()
            .collect()
    }
}

/// What `set` would change, from the item's current permissions.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Plan {
    /// People to grant, who have no permission of their own on the item yet.
    pub add: Vec<(String, Role)>,
    /// People who have a permission with a different role.
    pub change: Vec<(Grant, Role)>,
    /// Everything else, except the site owners: site groups, other people, groups, sharing links.
    pub remove: Vec<Grant>,
    /// Already right, and the site owners.
    pub keep: Vec<Grant>,
}

impl Plan {
    pub fn is_empty(&self) -> bool {
        self.add.is_empty() && self.change.is_empty() && self.remove.is_empty()
    }
}

/// The difference between `current` and `want`. Pure, so it's tested without SharePoint.
pub fn plan(current: &[Grant], want: &Desired) -> Plan {
    let mut out = Plan::default();
    let mut matched: Vec<&str> = vec![];
    for g in current {
        if g.is_site_owners() {
            out.keep.push(g.clone());
            continue;
        }
        let person = want.people().find(|(p, _)| g.who.is_person(p) && !matched.contains(p));
        match person {
            Some((p, role)) => {
                matched.push(p);
                if g.role == Some(role) {
                    out.keep.push(g.clone());
                } else {
                    out.change.push((g.clone(), role));
                }
            }
            None => out.remove.push(g.clone()),
        }
    }
    for (p, role) in want.people() {
        if !matched.contains(&p) {
            out.add.push((p.to_string(), role));
        }
    }
    out
}

/// The item's permissions.
pub async fn list(graph: &GraphClient, drive: &str, item: &str) -> Result<Vec<Grant>> {
    Ok(graph.permissions(drive, item).await?.iter().map(Grant::from_permission).collect())
}

/// Whether the item has its own permissions rather than its parent's. Graph doesn't say, so this compares the two
/// lists: a best guess. `None` for the library root.
pub async fn has_own_permissions(graph: &GraphClient, drive: &str, item: &DriveItem, grants: &[Grant]) -> Result<Option<bool>> {
    let Some(parent) = item.parent_id().filter(|_| !item.is_root()) else { return Ok(None) };
    let parent = list(graph, drive, parent).await?;
    let key = |g: &[Grant]| {
        let mut v: Vec<(String, Vec<String>)> = g.iter().map(|g| (g.id.clone(), g.roles.clone())).collect();
        v.sort();
        v
    };
    Ok(Some(key(grants) != key(&parent)))
}

/// What [`apply`] did.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Applied {
    pub granted: Vec<String>,
    pub changed: Vec<String>,
    pub removed: Vec<String>,
    /// The permissions afterwards.
    pub after: Vec<Grant>,
}

/// Why [`apply`] stopped partway. Nobody lost access that they should keep: grants happen first, removals last.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct Stopped {
    pub message: String,
    pub done: Applied,
}

/// Make `want` (plus the site owners) the only people with access to `item`.
///
/// 1. Grant every wanted person (this also gives the item its own permissions, copied from what it inherited).
///    With nobody wanted, the signed-in user is granted read for this step, and removed again in step 3.
/// 2. Read the permissions again, and stop if a wanted person isn't there.
/// 3. Change roles, then remove everything else. A refusal stops here (removing needs a site owner).
///
/// `me` is the signed-in user's user principal name.
pub async fn apply(graph: &GraphClient, drive: &str, item: &str, want: &Desired, notify: bool, me: &str) -> Result<Applied> {
    let mut done = Applied::default();
    let stop = |message: String, done: Applied| anyhow::Error::new(Stopped { message, done });

    let grants: Vec<(String, Role)> = if want.is_empty() {
        vec![(me.to_lowercase(), Role::Read)]
    } else {
        want.people().map(|(p, r)| (p.to_string(), r)).collect()
    };
    for (person, role) in &grants {
        if let Err(e) = graph.invite(drive, item, person, role.as_str(), notify && !want.is_empty()).await {
            let msg = format!("granting {person} {}: {e}. Nothing was removed", role.as_str());
            return Err(stop(msg, done));
        }
        if !want.is_empty() {
            done.granted.push(format!("{person} ({})", role.as_str()));
        }
    }

    let now = list(graph, drive, item).await?;
    let p = plan(&now, want);
    if let Some((missing, _)) = p.add.first() {
        let msg = format!(
            "SharePoint doesn't list {missing} on the item after granting access (is that their sign-in name? an email \
             alias won't match). Nothing was removed"
        );
        return Err(stop(msg, done));
    }
    for (g, role) in &p.change {
        if let Err(e) = graph.update_permission(drive, item, &g.id, role.as_str()).await {
            return Err(stop(format!("changing {} to {}: {e}. Nothing was removed", g.who.describe(), role.as_str()), done));
        }
        done.changed.push(format!("{} → {}", g.who.describe(), role.as_str()));
    }
    for g in &p.remove {
        match graph.delete_permission(drive, item, &g.id).await {
            Ok(()) => done.removed.push(g.who.describe()),
            Err(GraphError::NotFound(_)) => {}
            Err(e) => {
                let why = if e.is_forbidden() {
                    "removing access needs a site owner (you can grant, but not remove)".to_string()
                } else {
                    e.to_string()
                };
                let msg = if done.removed.is_empty() {
                    format!("{why}. Access was added, nothing was removed")
                } else {
                    format!("removing {}: {why}. Stopped after removing {}", g.who.describe(), done.removed.join(", "))
                };
                return Err(stop(msg, done));
            }
        }
    }
    done.after = list(graph, drive, item).await?;
    Ok(done)
}

/// What the item's permissions say about one person.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Verdict {
    /// From a permission naming them (directly, or a sharing link given to them).
    pub direct: Option<Role>,
    /// Groups and site groups with access. Whether the person is in them isn't readable with this app's
    /// permissions, so access through them is only possible, not certain.
    pub via_groups: Vec<(String, Role)>,
}

impl Verdict {
    pub fn best(&self) -> Option<Role> {
        self.direct
    }
}

/// [`Verdict`] for `person` (email address or user principal name) from the item's permissions.
pub fn check(grants: &[Grant], person: &str) -> Verdict {
    let mut v = Verdict::default();
    for g in grants {
        let Some(role) = g.role else { continue };
        let names_them = match &g.who {
            Who::Person { .. } => g.who.is_person(person),
            Who::Link { people, .. } => people.iter().any(|p| p.eq_ignore_ascii_case(person)),
            _ => false,
        };
        if names_them {
            v.direct = v.direct.max(Some(role));
        } else if let Who::Group { name } | Who::SiteGroup { name, .. } = &g.who {
            v.via_groups.push((name.clone(), role));
        }
    }
    v
}

/// What the signed-in user can do, found by trying: the item could be read (else this isn't reached), and write is
/// tested with an upload session that's cancelled at once.
pub async fn check_me(graph: &GraphClient, drive: &str, item: &DriveItem) -> Result<Role> {
    Ok(if graph.can_write(drive, item).await? { Role::Write } else { Role::Read })
}

/// A folder or file, found from a local path or a path in a library.
#[derive(Debug, Clone)]
pub struct Located {
    pub library: Library,
    pub drive_id: String,
    pub item: DriveItem,
    /// The path inside the library, `/`-separated (empty for the root).
    pub path: String,
}

/// Where to find the item: a local path inside a synced library folder, or a library (`local` name) and a path
/// in it.
pub enum Target<'a> {
    Local(&'a Path),
    InLibrary { library: &'a str, path: &'a str },
}

pub async fn locate(graph: &GraphClient, cfg: &Config, data_dir: &Path, target: Target<'_>) -> Result<Located> {
    let (lib, rel) = match target {
        Target::InLibrary { library, path } => {
            let lib = cfg.library(library).with_context(|| format!("no library with local = {library:?} in config"))?;
            (lib, path.trim_matches('/').to_string())
        }
        Target::Local(p) => library_for(cfg, p)?,
    };
    let resolved = match crate::index::cached(data_dir, lib) {
        Some(r) => r,
        None => crate::resolve(graph, lib).await?,
    };
    let drive_id = resolved.drive_id.clone();
    // The sync state knows the item exactly (names can differ locally); otherwise map the names back.
    let db = resolved.state_db(data_dir);
    let known = if db.exists() && !rel.is_empty() {
        StateDb::open(&db).ok().and_then(|d| d.by_local_key(&local_key(&rel)).ok().flatten())
    } else {
        None
    };
    let id = match known {
        Some(row) => row.item_id,
        None if rel.is_empty() => resolved.folder_id.clone().unwrap_or_else(|| "root".to_string()),
        None => {
            let map = cfg.char_map()?;
            let mut remote: Vec<String> = lib.folder.iter().map(|f| f.to_string()).collect();
            remote.extend(rel.split('/').map(|s| map.to_remote(s)));
            match graph.item_by_path(&drive_id, &remote.join("/")).await {
                Ok(it) => it.id,
                Err(GraphError::NotFound(_)) => bail!("{rel:?} isn't in library {:?} (or you can't see it)", lib.local),
                Err(e) => return Err(e.into()),
            }
        }
    };
    let item = graph.item_for_links(&drive_id, &id).await?;
    Ok(Located { library: lib.clone(), drive_id, item, path: rel })
}

/// The library whose local folder contains `p`, and `p`'s path inside it.
fn library_for<'c>(cfg: &'c Config, p: &Path) -> Result<(&'c Library, String)> {
    let abs = std::path::absolute(p)?;
    let abs = abs.canonicalize().unwrap_or(abs);
    for lib in &cfg.libraries {
        let root = cfg.local_root(lib);
        for r in [root.clone(), root.canonicalize().unwrap_or(root)] {
            if let Ok(rest) = abs.strip_prefix(&r) {
                let rel: Vec<String> = rest.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
                return Ok((lib, rel.join("/")));
            }
        }
    }
    bail!(
        "{} isn't inside a synced library folder. Use --library NAME PATH for a path in SharePoint \
         (`looper-sharepoint status` lists the libraries and their folders)",
        p.display()
    )
}

/// Links to SharePoint's own pages for an item, for what Graph can't do (or does better in the browser).
#[derive(Debug, Clone, Default, Serialize)]
pub struct Links {
    /// The folder or file itself.
    pub item: Option<String>,
    /// Advanced permissions: stop or restore inheritance ("Delete unique permissions"), grant to groups.
    pub permissions: Option<String>,
    /// Version history (files).
    pub versions: Option<String>,
    pub recycle_bin: Option<String>,
    /// The site's groups and their permissions.
    pub site_permissions: Option<String>,
    pub library_settings: Option<String>,
    pub site: Option<String>,
}

impl Links {
    pub fn for_item(item: &DriveItem) -> Links {
        let ids = item.sharepoint_ids.clone().unwrap_or_default();
        let mut l = Links { item: item.web_url.clone(), ..Default::default() };
        let Some(site) = ids.site_url.as_deref().map(|s| s.trim_end_matches('/')) else { return l };
        let layouts = format!("{site}/_layouts/15");
        l.site = Some(site.to_string());
        l.recycle_bin = Some(format!("{layouts}/RecycleBin.aspx"));
        l.site_permissions = Some(format!("{layouts}/user.aspx"));
        if let Some(list) = ids.list_id.as_deref() {
            let list = format!("%7B{list}%7D");
            l.library_settings = Some(format!("{layouts}/listedit.aspx?List={list}"));
            l.permissions = Some(match ids.list_item_id.as_deref().filter(|_| !item.is_root()) {
                Some(n) => format!("{layouts}/user.aspx?obj={list},{n},LISTITEM&List={list}"),
                None => format!("{layouts}/user.aspx?obj={list},doclib&List={list}"),
            });
            if let (Some(n), false) = (ids.list_item_id.as_deref(), item.is_dir()) {
                l.versions = Some(format!("{layouts}/Versions.aspx?list={list}&ID={n}"));
            }
        }
        l
    }

    /// The members page of a site group (`id` from [`Who::SiteGroup`]).
    pub fn site_group(item: &DriveItem, id: &str) -> Option<String> {
        let site = item.sharepoint_ids.as_ref()?.site_url.as_deref()?.trim_end_matches('/');
        Some(format!("{site}/_layouts/15/people.aspx?MembershipGroupId={id}"))
    }
}

/// Where the local copy of a library item is, if it's synced.
pub fn local_path(cfg: &Config, located: &Located) -> PathBuf {
    let mut p = cfg.local_root(&located.library);
    for seg in located.path.split('/').filter(|s| !s.is_empty()) {
        p.push(seg);
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    fn person(id: &str, email: &str, role: Role) -> Grant {
        Grant {
            id: id.into(),
            who: Who::Person { name: email.into(), email: Some(email.into()), login: Some(email.into()) },
            role: Some(role),
            roles: vec![role.as_str().into()],
        }
    }

    fn site_group(id: &str, name: &str, role: Role) -> Grant {
        Grant { id: id.into(), who: Who::SiteGroup { name: name.into(), id: Some("3".into()) }, role: Some(role), roles: vec![role.as_str().into()] }
    }

    fn inherited() -> Vec<Grant> {
        vec![
            site_group("o", "Team Owners", Role::Owner),
            Grant { id: "g".into(), who: Who::Group { name: "Team Owners".into() }, role: Some(Role::Owner), roles: vec!["owner".into()] },
            site_group("v", "Team Visitors", Role::Read),
            site_group("m", "Team Members", Role::Write),
        ]
    }

    fn ids(gs: &[Grant]) -> Vec<&str> {
        gs.iter().map(|g| g.id.as_str()).collect()
    }

    #[test]
    fn keeps_owners_removes_site_groups_adds_people() {
        let want = Desired::new(&["ann@contoso.com".into()], &["bob@contoso.com".into()]).unwrap();
        let p = plan(&inherited(), &want);
        assert_eq!(ids(&p.keep), ["o", "g"]);
        assert_eq!(ids(&p.remove), ["v", "m"]);
        assert_eq!(p.add, [("ann@contoso.com".to_string(), Role::Read), ("bob@contoso.com".to_string(), Role::Write)]);
        assert!(p.change.is_empty());
    }

    #[test]
    fn already_right_is_nothing_to_do() {
        let mut now = vec![site_group("o", "Team Owners", Role::Owner)];
        now.push(person("a", "Ann@Contoso.com", Role::Read));
        let want = Desired::new(&["ann@contoso.com".into()], &[]).unwrap();
        assert!(plan(&now, &want).is_empty());
    }

    #[test]
    fn changes_roles_and_removes_others_and_links() {
        let mut now = inherited();
        now.push(person("a", "ann@contoso.com", Role::Write));
        now.push(person("c", "cat@contoso.com", Role::Write));
        now.push(Grant {
            id: "l".into(),
            who: Who::Link { scope: "organization".into(), kind: "view".into(), people: vec![] },
            role: Some(Role::Read),
            roles: vec!["read".into()],
        });
        let want = Desired::new(&["ann@contoso.com".into()], &[]).unwrap();
        let p = plan(&now, &want);
        assert_eq!(p.change.len(), 1);
        assert_eq!((p.change[0].0.id.as_str(), p.change[0].1), ("a", Role::Read));
        assert_eq!(ids(&p.remove), ["v", "m", "c", "l"]);
        assert!(p.add.is_empty());
    }

    #[test]
    fn both_lists_means_write_and_duplicates_are_removed() {
        let want = Desired::new(&["ann@contoso.com".into()], &["ANN@contoso.com".into()]).unwrap();
        assert_eq!(want.people().collect::<Vec<_>>(), [("ann@contoso.com", Role::Write)]);
        let now = vec![person("a1", "ann@contoso.com", Role::Write), person("a2", "ann@contoso.com", Role::Read)];
        let p = plan(&now, &want);
        assert_eq!(ids(&p.keep), ["a1"]);
        assert_eq!(ids(&p.remove), ["a2"]);
    }

    #[test]
    fn nobody_wanted_leaves_only_owners() {
        let p = plan(&inherited(), &Desired::default());
        assert_eq!(ids(&p.keep), ["o", "g"]);
        assert_eq!(ids(&p.remove), ["v", "m"]);
    }

    #[test]
    fn outside_people() {
        let want = Desired::new(&["ann@contoso.com".into(), "eve@example.org".into()], &[]).unwrap();
        assert_eq!(want.outside(&["contoso.com".into()]), ["eve@example.org"]);
        assert!(Desired::new(&["ann".into()], &[]).is_err());
    }

    #[test]
    fn check_direct_and_groups() {
        let mut now = inherited();
        now.push(person("a", "ann@contoso.com", Role::Read));
        let v = check(&now, "ANN@contoso.com");
        assert_eq!(v.direct, Some(Role::Read));
        assert_eq!(v.via_groups.len(), 4);
        assert_eq!(check(&now, "bob@contoso.com").direct, None);
    }

    #[test]
    fn guest_matches_by_email() {
        // As SharePoint lists a guest: no email, the tenant's guest sign-in name.
        let who = Who::Person { name: "Eve".into(), email: None, login: Some("eve_example.org#ext#@contoso.com".into()) };
        assert!(who.is_person("Eve@example.org"));
        assert!(!who.is_person("eve@example.com"));
    }

    #[test]
    fn parses_graph_permissions() {
        let p: Permission = serde_json::from_value(serde_json::json!({
            "id": "c2hh", "roles": ["owner"], "inheritedFrom": {},
            "grantedToV2": { "siteGroup": { "displayName": "QA Owners", "id": "3", "loginName": "QA Owners" } }
        }))
        .unwrap();
        let g = Grant::from_permission(&p);
        assert!(g.is_site_owners());
        let p: Permission = serde_json::from_value(serde_json::json!({
            "id": "u1", "roles": ["write"],
            "grantedToV2": {
                "user": { "displayName": "Ann", "email": "Ann@contoso.com", "id": "x" },
                "siteUser": { "displayName": "Ann", "id": "12", "loginName": "i:0#.f|membership|ann@contoso.com" }
            }
        }))
        .unwrap();
        let g = Grant::from_permission(&p);
        assert_eq!(g.role, Some(Role::Write));
        assert!(g.who.is_person("ann@contoso.com"));
        assert_eq!(g.who.describe(), "Ann <Ann@contoso.com>");
    }

    #[test]
    fn links_into_sharepoint() {
        let it: DriveItem = serde_json::from_value(serde_json::json!({
            "id": "i", "name": "a.txt", "file": {}, "webUrl": "https://c.sharepoint.com/sites/t/Shared%20Documents/a.txt",
            "sharepointIds": { "listId": "L", "listItemId": "8", "siteUrl": "https://c.sharepoint.com/sites/t" }
        }))
        .unwrap();
        let l = Links::for_item(&it);
        assert_eq!(l.permissions.as_deref(), Some("https://c.sharepoint.com/sites/t/_layouts/15/user.aspx?obj=%7BL%7D,8,LISTITEM&List=%7BL%7D"));
        assert_eq!(l.versions.as_deref(), Some("https://c.sharepoint.com/sites/t/_layouts/15/Versions.aspx?list=%7BL%7D&ID=8"));
        assert_eq!(l.recycle_bin.as_deref(), Some("https://c.sharepoint.com/sites/t/_layouts/15/RecycleBin.aspx"));
    }
}
