//! What a pasted link points to, for `add`: a SharePoint site, library or folder link (anything copied from the
//! browser, Teams' "Open in SharePoint", or a sharing link), and `looper-sharepoint://` links.
//! See `specs/onboarding-design.md` §5 and §7.

use anyhow::{Context, Result, bail};
use looper_sharepoint_graph::models::DriveItem;
use looper_sharepoint_graph::{GraphClient, GraphError};

/// The scheme of the app's own links.
pub const SCHEME: &str = "looper-sharepoint";

/// A library, or a folder in one, that a link points to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    /// For the config's `site`.
    pub site_url: String,
    pub site_name: String,
    /// For the config's `library`: the library's name.
    pub library: String,
    pub drive_id: String,
    /// The folder's path in the library (`/`-separated), or `None` for the whole library.
    pub folder: Option<String>,
    /// The site's other document libraries, when the link was to a site and one was chosen for it.
    pub other_libraries: Vec<String>,
}

impl Found {
    /// The suggested local folder name: the folder's name for a folder, else the site's (plus the library's, when it
    /// isn't the site's usual "Documents").
    pub fn suggested_name(&self) -> String {
        match &self.folder {
            Some(f) => slug(f.rsplit('/').next().unwrap_or(f)),
            None if is_default_library(&self.library) => slug(&self.site_name),
            None => format!("{}-{}", slug(&self.site_name), slug(&self.library)),
        }
    }

    /// "Documents in Engineering", "Specs in Engineering / Documents".
    pub fn describe(&self) -> String {
        match &self.folder {
            Some(f) => format!("folder \"{f}\" in {} / {}", self.site_name, self.library),
            None => format!("library \"{}\" in {}", self.library, self.site_name),
        }
    }
}

fn is_default_library(name: &str) -> bool {
    ["documents", "shared documents"].contains(&name.to_lowercase().as_str())
}

/// A folder-name version of `s`: lowercase, words joined with `-`, only letters, digits, `-`, `_` and `.`.
pub fn slug(s: &str) -> String {
    let mut out = String::new();
    for c in s.trim().chars() {
        if c.is_alphanumeric() || c == '_' || c == '.' {
            out.extend(c.to_lowercase());
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    let out = out.trim_matches(['-', '.']).to_string();
    if out.is_empty() { "sharepoint".into() } else { out }
}

/// Find what `link` points to. `library` picks one when the link is to a site with several.
pub async fn find(graph: &GraphClient, link: &str, library: Option<&str>) -> Result<Found> {
    let link = link.trim();
    let u = url::Url::parse(link).with_context(|| format!("{link:?} isn't a link; copy the address from the browser"))?;
    if u.scheme() != "https" || !u.host_str().is_some_and(|h| h.to_lowercase().contains("sharepoint")) {
        bail!("{link:?} isn't a SharePoint link (https://<company>.sharepoint.com/…)");
    }
    match graph.shared_item(link).await {
        Ok(item) => from_item(graph, link, item).await,
        // A link to a site isn't an item; anything else that's refused may still be a site.
        Err(GraphError::Http { status: 400 | 404, .. }) | Err(GraphError::NotFound(_)) => match graph.shared_site(link).await {
            Ok(site) => from_site(graph, site, library).await,
            Err(e) if e.is_forbidden() => bail!("SharePoint says you don't have access to {link}"),
            Err(GraphError::Http { status: 400 | 404, .. }) | Err(GraphError::NotFound(_)) => {
                bail!("SharePoint can't find {link}: it may have been moved or deleted, or you can't see it")
            }
            Err(e) => Err(anyhow::Error::from(e).context(format!("looking up {link}"))),
        },
        Err(e) if e.is_forbidden() => bail!("SharePoint says you don't have access to {link}"),
        Err(e) => Err(anyhow::Error::from(e).context(format!("looking up {link}"))),
    }
}

async fn from_item(graph: &GraphClient, link: &str, item: DriveItem) -> Result<Found> {
    if item.file.is_some() {
        bail!("that's a link to a file ({}); paste the link to its folder instead", item.name.unwrap_or_default());
    }
    let parent = item.parent_reference.as_ref();
    let drive_id = parent.and_then(|p| p.drive_id.clone()).context("SharePoint didn't say which library that's in")?;
    let folder = if item.is_root() {
        None
    } else {
        // `/drives/<id>/root:/Projects` for an item in `Projects`; `/drives/<id>/root:` at the top.
        let parent_path = parent.and_then(|p| p.path.as_deref()).and_then(|p| p.split_once("root:")).map(|(_, rest)| rest).unwrap_or("");
        let name = item.name.clone().unwrap_or_default();
        Some(format!("{}/{name}", parent_path.trim_matches('/')).trim_start_matches('/').to_string())
    };
    let site_url = item
        .sharepoint_ids
        .as_ref()
        .and_then(|s| s.site_url.clone())
        .with_context(|| format!("SharePoint didn't say which site {link} is in"))?;
    let drive = graph.drive(&drive_id).await?;
    let site = graph.site_by_url(&site_url).await?;
    Ok(Found {
        site_name: site.display_name.or(site.name).unwrap_or_else(|| site_url.clone()),
        site_url: site_url.trim_end_matches('/').to_string(),
        library: drive.name.unwrap_or_else(|| "Documents".into()),
        drive_id,
        folder,
        other_libraries: vec![],
    })
}

async fn from_site(graph: &GraphClient, site: looper_sharepoint_graph::models::Site, library: Option<&str>) -> Result<Found> {
    let site_url = site.web_url.clone().context("SharePoint didn't give the site's address")?;
    let site_name = site.display_name.clone().or(site.name.clone()).unwrap_or_else(|| site_url.clone());
    let drives = graph.site_drives(&site.id).await?;
    let names: Vec<String> = drives.iter().filter_map(|d| d.name.clone()).collect();
    let drive = match library {
        Some(want) => drives
            .iter()
            .find(|d| d.name.as_deref().is_some_and(|n| n.eq_ignore_ascii_case(want)))
            .cloned()
            .with_context(|| format!("{site_name} has no library {want:?} (it has: {})", names.join(", ")))?,
        None if drives.len() == 1 => drives[0].clone(),
        None => graph.site_default_drive(&site.id).await?,
    };
    let name = drive.name.clone().unwrap_or_else(|| "Documents".into());
    Ok(Found {
        site_url: site_url.trim_end_matches('/').to_string(),
        site_name,
        other_libraries: names.into_iter().filter(|n| !n.eq_ignore_ascii_case(&name)).collect(),
        library: name,
        drive_id: drive.id,
        folder: None,
    })
}

/// A `looper-sharepoint://` link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppLink {
    /// `looper-sharepoint://setup?tenant=…&client_id=…&app_name=…&publisher=…`
    Setup { tenant: String, client_id: String, app_name: Option<String>, publisher: Option<String> },
    /// `looper-sharepoint://add?url=<SharePoint link>&to=<suggested folder>`
    Add { url: String, to: Option<String> },
}

/// Parse a `looper-sharepoint://` link; `Ok(None)` for anything else. Unknown actions and keys are refused, so a link
/// from a newer version never half-works.
pub fn parse_app_link(s: &str) -> Result<Option<AppLink>> {
    let s = s.trim();
    if !s.to_lowercase().starts_with(&format!("{SCHEME}:")) {
        return Ok(None);
    }
    let u = url::Url::parse(s).with_context(|| format!("{s:?} isn't a valid link"))?;
    let action = u.host_str().map(str::to_string).unwrap_or_else(|| u.path().trim_matches('/').to_string());
    let mut q: std::collections::BTreeMap<String, String> = u.query_pairs().map(|(k, v)| (k.into_owned(), v.into_owned())).collect();
    let mut take = |k: &str| q.remove(k).filter(|v| !v.trim().is_empty());
    let link = match action.to_lowercase().as_str() {
        "setup" => AppLink::Setup {
            tenant: take("tenant").context("the setup link has no tenant")?,
            client_id: take("client_id").context("the setup link has no client_id")?,
            app_name: take("app_name"),
            publisher: take("publisher"),
        },
        "add" => AppLink::Add { url: take("url").context("the add link has no url")?, to: take("to") },
        other => bail!("this version doesn't know {SCHEME}://{other} links; update looper-sharepoint"),
    };
    if let Some(k) = q.keys().next() {
        bail!("this version doesn't know `{k}` in {SCHEME}:// links; update looper-sharepoint");
    }
    Ok(Some(link))
}

/// The `looper-sharepoint://setup` link for these values.
pub fn setup_link(tenant: &str, client_id: &str, app_name: Option<&str>, publisher: Option<&str>) -> String {
    let mut q = url::form_urlencoded::Serializer::new(String::new());
    q.append_pair("tenant", tenant).append_pair("client_id", client_id);
    if let Some(a) = app_name {
        q.append_pair("app_name", a);
    }
    if let Some(p) = publisher {
        q.append_pair("publisher", p);
    }
    format!("{SCHEME}://setup?{}", q.finish().replace('+', "%20"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs() {
        assert_eq!(slug("Engineering Team"), "engineering-team");
        assert_eq!(slug("  R&D / Plans "), "r-d-plans");
        assert_eq!(slug("Q3_2026.v2"), "q3_2026.v2");
        assert_eq!(slug("..."), "sharepoint");
    }

    #[test]
    fn suggested_names() {
        let f = |library: &str, folder: Option<&str>| Found {
            site_url: "https://contoso.sharepoint.com/sites/eng".into(),
            site_name: "Engineering Team".into(),
            library: library.into(),
            drive_id: "d".into(),
            folder: folder.map(Into::into),
            other_libraries: vec![],
        };
        assert_eq!(f("Documents", None).suggested_name(), "engineering-team");
        assert_eq!(f("Shared Documents", None).suggested_name(), "engineering-team");
        assert_eq!(f("Design Files", None).suggested_name(), "engineering-team-design-files");
        assert_eq!(f("Documents", Some("Projects/Board Minutes")).suggested_name(), "board-minutes");
    }

    #[test]
    fn app_links() {
        let l = setup_link("t-1", "c-2", Some("Contoso Sync"), None);
        assert_eq!(l, "looper-sharepoint://setup?tenant=t-1&client_id=c-2&app_name=Contoso%20Sync");
        assert_eq!(
            parse_app_link(&l).unwrap(),
            Some(AppLink::Setup { tenant: "t-1".into(), client_id: "c-2".into(), app_name: Some("Contoso Sync".into()), publisher: None })
        );
        let add = "looper-sharepoint://add?url=https%3A%2F%2Fcontoso.sharepoint.com%2Fsites%2Feng&to=~%2Fwork%2Feng";
        assert_eq!(
            parse_app_link(add).unwrap(),
            Some(AppLink::Add { url: "https://contoso.sharepoint.com/sites/eng".into(), to: Some("~/work/eng".into()) })
        );
        assert_eq!(parse_app_link("https://contoso.sharepoint.com").unwrap(), None);
        assert!(parse_app_link("looper-sharepoint://delete?all=1").is_err());
        assert!(parse_app_link("looper-sharepoint://setup?tenant=t&client_id=c&direction=download_only").is_err());
        assert!(parse_app_link("looper-sharepoint://setup?tenant=t").is_err());
    }
}
