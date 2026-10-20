//! Endpoints used by looper-sharepoint.

use bytes::Bytes;
use futures_util::Stream;
use reqwest::{Method, StatusCode};
use serde_json::json;

use crate::client::{GraphClient, GraphError, Result, decode};
use crate::models::*;

/// Graph path segment encoding for `root:/path:` addressing.
fn enc_path(p: &str) -> String {
    p.split('/')
        .map(|seg| url::form_urlencoded::byte_serialize(seg.as_bytes()).collect::<String>().replace('+', "%20"))
        .collect::<Vec<_>>()
        .join("/")
}

/// `u!` + unpadded base64url of the link: how `/shares` takes a URL.
fn share_id(link: &str) -> String {
    use base64::Engine;
    format!("u!{}", base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(link.as_bytes()))
}

/// Upload chunk size: a multiple of 320 KiB, ~10 MiB.
pub const CHUNK: usize = 320 * 1024 * 32;

/// Where [`GraphClient::upload_file`] writes.
pub enum UploadTarget<'a> {
    /// Replace the content of this item (in place).
    Existing { item_id: &'a str },
    /// Create `name` in folder `parent_id`; fail if the name is taken.
    New { parent_id: &'a str, name: &'a str },
}
/// Files up to this size use a single PUT.
const SIMPLE_UPLOAD_MAX: usize = 4 * 1024 * 1024;

impl GraphClient {
    pub async fn me(&self) -> Result<User> {
        self.get_json("/me?$select=userPrincipalName,displayName,mail").await
    }

    /// Resolve `https://host/sites/x` (or the root site `https://host`) to a site.
    pub async fn site_by_url(&self, site_url: &str) -> Result<Site> {
        let u = url::Url::parse(site_url).map_err(|e| GraphError::Decode(format!("site URL {site_url:?}: {e}")))?;
        let host = u.host_str().ok_or_else(|| GraphError::Decode(format!("site URL {site_url:?} has no host")))?;
        let path = u.path().trim_end_matches('/');
        let p = if path.is_empty() { format!("/sites/{host}") } else { format!("/sites/{host}:{path}") };
        self.get_json(&format!("{p}?$select=id,displayName,name,webUrl")).await
    }

    /// The item a SharePoint link points to: a sharing link, or a plain link to a folder, library or file
    /// (including the browser's `Forms/AllItems.aspx?id=…` pages). <https://learn.microsoft.com/graph/api/shares-get>
    pub async fn shared_item(&self, link: &str) -> Result<DriveItem> {
        self.get_json(&format!(
            "/shares/{}/driveItem?$select=id,name,folder,file,root,parentReference,webUrl,sharepointIds",
            share_id(link)
        ))
        .await
    }

    /// The site a SharePoint link points to (for a link to a site, which [`shared_item`](Self::shared_item) refuses).
    pub async fn shared_site(&self, link: &str) -> Result<Site> {
        self.get_json(&format!("/shares/{}/site?$select=id,displayName,name,webUrl", share_id(link))).await
    }

    pub async fn drive(&self, drive_id: &str) -> Result<Drive> {
        self.get_json(&format!("/drives/{drive_id}?$select=id,name,driveType,webUrl")).await
    }

    /// The site's default document library.
    pub async fn site_default_drive(&self, site_id: &str) -> Result<Drive> {
        self.get_json(&format!("/sites/{site_id}/drive?$select=id,name,driveType,webUrl")).await
    }

    pub async fn site(&self, site_id: &str) -> Result<Site> {
        self.get_json(&format!("/sites/{site_id}?$select=id,displayName,name,webUrl")).await
    }

    pub async fn site_drives(&self, site_id: &str) -> Result<Vec<Drive>> {
        self.collect(&format!("/sites/{site_id}/drives?$select=id,name,driveType,webUrl")).await
    }

    pub async fn followed_sites(&self) -> Result<Vec<Site>> {
        self.collect("/me/followedSites?$select=id,displayName,name,webUrl").await
    }

    pub async fn search_sites(&self, query: &str) -> Result<Vec<Site>> {
        let q: String = url::form_urlencoded::byte_serialize(query.as_bytes()).collect();
        self.collect(&format!("/sites?search={q}&$select=id,displayName,name,webUrl")).await
    }

    async fn collect<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<Vec<T>> {
        let mut out = vec![];
        let mut next = Some(path.to_string());
        while let Some(p) = next {
            let page: Collection<T> = self.get_json(&p).await?;
            out.extend(page.value);
            next = page.next_link;
        }
        Ok(out)
    }

    /// One page of delta. `link` is a nextLink/deltaLink from a previous page, or `None` to start a full enumeration.
    /// Delta always runs at the library root (SharePoint has no folder-scoped delta).
    pub async fn delta_page(&self, drive_id: &str, link: Option<&str>) -> Result<DeltaPage> {
        let path = match link {
            Some(l) => l.to_string(),
            None => format!(
                "/drives/{drive_id}/root/delta?$select=id,name,eTag,size,parentReference,file,folder,root,deleted,fileSystemInfo,lastModifiedDateTime"
            ),
        };
        self.get_json(&path).await
    }

    /// A deltaLink for "now", without enumerating (`token=latest`).
    pub async fn delta_latest(&self, drive_id: &str) -> Result<String> {
        let page = self.delta_page(drive_id, Some(&format!("/drives/{drive_id}/root/delta?token=latest"))).await?;
        page.delta_link.ok_or_else(|| GraphError::Decode("token=latest returned no deltaLink".into()))
    }

    /// A socket.io endpoint that announces changes to this drive (delegated auth only). The URL is pre-authenticated
    /// and Microsoft-hosted; see `push`.
    pub async fn socket_io_endpoint(&self, drive_id: &str) -> Result<SocketIoSubscription> {
        self.get_json(&format!("/drives/{drive_id}/root/subscriptions/socketIo")).await
    }

    pub async fn item(&self, drive_id: &str, item_id: &str) -> Result<DriveItem> {
        self.get_json(&format!("/drives/{drive_id}/items/{item_id}")).await
    }

    pub async fn item_by_path(&self, drive_id: &str, path: &str) -> Result<DriveItem> {
        self.get_json(&format!("/drives/{drive_id}/root:/{}", enc_path(path))).await
    }

    pub async fn children(&self, drive_id: &str, item_id: &str) -> Result<Vec<DriveItem>> {
        self.collect(&format!("/drives/{drive_id}/items/{item_id}/children")).await
    }

    /// Stream a file's content. Graph answers with a 302 to a pre-authenticated URL; that URL is fetched without our
    /// bearer token.
    pub async fn download(&self, drive_id: &str, item_id: &str) -> Result<impl Stream<Item = reqwest::Result<Bytes>> + use<>> {
        let resp = self.send(Method::GET, &format!("/drives/{drive_id}/items/{item_id}/content"), |r| r).await?;
        let resp = if resp.status().is_redirection() {
            let loc = resp
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| GraphError::Decode("download redirect without Location".into()))?
                .to_string();
            self.send_plain(&loc, crate::client::RequestKind::Download, |c| c.get(&loc)).await?
        } else {
            resp
        };
        Ok(resp.bytes_stream())
    }

    /// Upload `data` to `parent_path/name` (path relative to the library root), replacing any existing file.
    /// Small files use one PUT; larger ones an upload session. Used by test fixtures in M1; M4 adds `If-Match`.
    pub async fn upload(&self, drive_id: &str, rel_path: &str, data: Bytes) -> Result<DriveItem> {
        let target = format!("/drives/{drive_id}/root:/{}:", enc_path(rel_path));
        if data.len() <= SIMPLE_UPLOAD_MAX {
            let resp = self
                .send(Method::PUT, &format!("{target}/content?@microsoft.graph.conflictBehavior=replace"), |r| {
                    r.header("Content-Type", "application/octet-stream").body(data.clone())
                })
                .await?;
            return decode(resp).await;
        }
        let body = json!({ "item": { "@microsoft.graph.conflictBehavior": "replace" } });
        let resp = self.send(Method::POST, &format!("{target}/createUploadSession"), |r| r.json(&body)).await?;
        let session: UploadSession = decode(resp).await?;
        let total = data.len();
        let mut start = 0;
        loop {
            let end = (start + CHUNK).min(total);
            let chunk = data.slice(start..end);
            let url = session.upload_url.clone();
            let resp = self
                .send_plain(&url, crate::client::RequestKind::Upload, |c| {
                    c.put(&url)
                        .header("Content-Length", chunk.len())
                        .header("Content-Range", format!("bytes {start}-{}/{total}", end - 1))
                        .body(chunk.clone())
                })
                .await?;
            if end == total {
                if resp.status() == StatusCode::OK || resp.status() == StatusCode::CREATED {
                    return decode(resp).await;
                }
                return Err(GraphError::Decode(format!("upload session finished with {}", resp.status())));
            }
            start = end;
        }
    }

    /// Upload a local file (two-way sync). Content is streamed from disk in [`CHUNK`]-sized pieces through an upload
    /// session (a simple PUT for empty files); `observe` sees every byte sent, so the caller can hash what was
    /// actually uploaded.
    ///
    /// - [`UploadTarget::Existing`] updates the item in place (keeping its id and version history). With `if_match`,
    ///   SharePoint refuses with 412 if the item changed since that eTag.
    /// - [`UploadTarget::New`] creates a file and fails with 409 if the name is taken; it never replaces anything.
    ///
    /// `modified` (RFC 3339) becomes the item's `fileSystemInfo.lastModifiedDateTime`.
    pub async fn upload_file(
        &self,
        drive_id: &str,
        target: &UploadTarget<'_>,
        path: &std::path::Path,
        modified: Option<&str>,
        if_match: Option<&str>,
        observe: &mut (dyn FnMut(&[u8]) + Send),
    ) -> Result<DriveItem> {
        use tokio::io::AsyncReadExt;
        let io = |e: std::io::Error| GraphError::Decode(format!("reading {}: {e}", path.display()));
        let mut file = tokio::fs::File::open(path).await.map_err(io)?;
        let total = file.metadata().await.map_err(io)?.len() as usize;
        let base = match target {
            UploadTarget::Existing { item_id } => format!("/drives/{drive_id}/items/{item_id}"),
            UploadTarget::New { parent_id, name } => format!("/drives/{drive_id}/items/{parent_id}:/{}:", enc_path(name)),
        };
        let behavior = match target {
            UploadTarget::Existing { .. } => "replace",
            UploadTarget::New { .. } => "fail",
        };

        if total == 0 {
            let resp = self
                .send(Method::PUT, &format!("{base}/content?@microsoft.graph.conflictBehavior={behavior}"), |r| {
                    let r = r.header("Content-Type", "application/octet-stream").header("Content-Length", 0).body(Vec::<u8>::new());
                    match if_match {
                        Some(e) => r.header("If-Match", e),
                        None => r,
                    }
                })
                .await?;
            return decode(resp).await;
        }

        let mut item = json!({ "@microsoft.graph.conflictBehavior": behavior });
        if let Some(m) = modified {
            item["fileSystemInfo"] = json!({ "lastModifiedDateTime": m });
        }
        let body = json!({ "item": item });
        let resp = self
            .send(Method::POST, &format!("{base}/createUploadSession"), |r| {
                let r = r.json(&body);
                match if_match {
                    Some(e) => r.header("If-Match", e),
                    None => r,
                }
            })
            .await?;
        let session: UploadSession = decode(resp).await?;
        let url = session.upload_url;
        let mut start = 0usize;
        let mut buf = vec![0u8; CHUNK];
        loop {
            let want = CHUNK.min(total - start);
            file.read_exact(&mut buf[..want]).await.map_err(io)?;
            observe(&buf[..want]);
            let chunk = Bytes::copy_from_slice(&buf[..want]);
            let end = start + want;
            let resp = self
                .send_plain(&url, crate::client::RequestKind::Upload, |c| {
                    c.put(&url)
                        .header("Content-Length", chunk.len())
                        .header("Content-Range", format!("bytes {start}-{}/{total}", end - 1))
                        .body(chunk.clone())
                })
                .await?;
            if end == total {
                if resp.status() == StatusCode::OK || resp.status() == StatusCode::CREATED {
                    return decode(resp).await;
                }
                return Err(GraphError::Decode(format!("upload session finished with {}", resp.status())));
            }
            start = end;
        }
    }

    /// Create a folder `name` inside `parent_id`. If one already exists with that name, it is returned instead
    /// (`existed = true`): a folder created on both sides is the same folder.
    pub async fn create_folder(&self, drive_id: &str, parent_id: &str, name: &str) -> Result<(DriveItem, bool)> {
        let body = json!({ "name": name, "folder": {}, "@microsoft.graph.conflictBehavior": "fail" });
        match self.send(Method::POST, &format!("/drives/{drive_id}/items/{parent_id}/children"), |r| r.json(&body)).await {
            Ok(resp) => Ok((decode(resp).await?, false)),
            Err(e) if e.is_conflict() => {
                let existing: DriveItem = self.get_json(&format!("/drives/{drive_id}/items/{parent_id}:/{}:", enc_path(name))).await?;
                if existing.is_dir() {
                    Ok((existing, true))
                } else {
                    Err(e)
                }
            }
            Err(e) => Err(e),
        }
    }

    /// Delete an item only if it is unchanged since `etag` (412 otherwise). Goes to the recycle bin.
    pub async fn delete_item_if(&self, drive_id: &str, item_id: &str, etag: Option<&str>) -> Result<()> {
        let r = self
            .send(Method::DELETE, &format!("/drives/{drive_id}/items/{item_id}"), |r| match etag {
                Some(e) => r.header("If-Match", e),
                None => r,
            })
            .await;
        match r {
            Ok(_) | Err(GraphError::NotFound(_)) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Create a folder (and any missing parents) at `rel_path`. Existing folders are returned as-is.
    pub async fn ensure_folder(&self, drive_id: &str, rel_path: &str) -> Result<DriveItem> {
        let mut parent = self.get_json::<DriveItem>(&format!("/drives/{drive_id}/root")).await?;
        for seg in rel_path.split('/').filter(|s| !s.is_empty()) {
            let body = json!({ "name": seg, "folder": {}, "@microsoft.graph.conflictBehavior": "fail" });
            let path = format!("/drives/{drive_id}/items/{}/children", parent.id);
            parent = match self.send(Method::POST, &path, |r| r.json(&body)).await {
                Ok(resp) => decode(resp).await?,
                Err(GraphError::Http { status: 409, .. }) => {
                    let p = parent.id.clone();
                    self.get_json(&format!("/drives/{drive_id}/items/{p}:/{}:", enc_path(seg))).await?
                }
                Err(e) => return Err(e),
            };
        }
        Ok(parent)
    }

    /// Create an empty file, failing if the name is taken by an item the caller can see. SharePoint renames it instead
    /// (`name (1).ext`) when the name is held by an item the caller can't see, so a different name in the result
    /// means one exists.
    pub async fn create_empty_file(&self, drive_id: &str, parent_id: &str, name: &str) -> Result<DriveItem> {
        let path = format!("/drives/{drive_id}/items/{parent_id}:/{}:/content?@microsoft.graph.conflictBehavior=fail", enc_path(name));
        let resp = self
            .send(Method::PUT, &path, |r| r.header("Content-Type", "application/octet-stream").header("Content-Length", 0).body(Vec::<u8>::new()))
            .await?;
        decode(resp).await
    }

    /// Delete an item. This goes to the SharePoint recycle bin; looper-sharepoint never calls permanentDelete.
    pub async fn delete_item(&self, drive_id: &str, item_id: &str) -> Result<()> {
        match self.send(Method::DELETE, &format!("/drives/{drive_id}/items/{item_id}"), |r| r).await {
            Ok(_) | Err(GraphError::NotFound(_)) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// An item with what links into SharePoint's own pages need (`webUrl`, `sharepointIds`).
    pub async fn item_for_links(&self, drive_id: &str, item_id: &str) -> Result<DriveItem> {
        self.get_json(&format!(
            "/drives/{drive_id}/items/{item_id}?$select=id,name,webUrl,parentReference,file,folder,root,sharepointIds"
        ))
        .await
    }

    /// Everyone with access to an item, as SharePoint reports it (site groups, people, groups, sharing links).
    /// <https://learn.microsoft.com/graph/api/driveitem-list-permissions>
    pub async fn permissions(&self, drive_id: &str, item_id: &str) -> Result<Vec<Permission>> {
        self.collect(&format!("/drives/{drive_id}/items/{item_id}/permissions")).await
    }

    /// Give one person access (`role` is `read` or `write`). Sign-in is required, so it never creates an anonymous
    /// link; an email goes out only with `notify`. The first grant on an item that inherits its permissions gives it
    /// its own (a copy of the inherited ones plus this grant). <https://learn.microsoft.com/graph/api/driveitem-invite>
    pub async fn invite(&self, drive_id: &str, item_id: &str, email: &str, role: &str, notify: bool) -> Result<Vec<Permission>> {
        let body = json!({
            "recipients": [{ "email": email }],
            "requireSignIn": true,
            "sendInvitation": notify,
            "roles": [role],
        });
        let resp = self.send(Method::POST, &format!("/drives/{drive_id}/items/{item_id}/invite"), |r| r.json(&body)).await?;
        Ok(decode::<Collection<Permission>>(resp).await?.value)
    }

    /// <https://learn.microsoft.com/graph/api/permission-update>
    pub async fn update_permission(&self, drive_id: &str, item_id: &str, permission_id: &str, role: &str) -> Result<()> {
        let body = json!({ "roles": [role] });
        self.send(Method::PATCH, &format!("/drives/{drive_id}/items/{item_id}/permissions/{permission_id}"), |r| r.json(&body)).await?;
        Ok(())
    }

    /// Remove one permission. Needs a site owner (403 otherwise).
    /// <https://learn.microsoft.com/graph/api/permission-delete>
    pub async fn delete_permission(&self, drive_id: &str, item_id: &str, permission_id: &str) -> Result<()> {
        self.send(Method::DELETE, &format!("/drives/{drive_id}/items/{item_id}/permissions/{permission_id}"), |r| r).await?;
        Ok(())
    }

    /// The organisation's verified domains (e.g. `contoso.com`), to tell people in it from outside people.
    pub async fn verified_domains(&self) -> Result<Vec<String>> {
        let orgs: Vec<Organization> = self.collect("/organization?$select=verifiedDomains").await?;
        Ok(orgs.into_iter().flat_map(|o| o.verified_domains).map(|d| d.name.to_lowercase()).collect())
    }

    /// Whether the signed-in user can write to this item, found without changing anything: start an upload session
    /// (into a folder, under a name that can't exist; or onto a file) and cancel it at once. Nothing is written until
    /// a session finishes. `Ok(false)` when SharePoint refuses it.
    pub async fn can_write(&self, drive_id: &str, item: &DriveItem) -> Result<bool> {
        let target = if item.is_dir() {
            let probe = format!(".looper-sharepoint-write-check-{}", std::process::id());
            format!("/drives/{drive_id}/items/{}:/{}:/createUploadSession", item.id, enc_path(&probe))
        } else {
            format!("/drives/{drive_id}/items/{}/createUploadSession", item.id)
        };
        // Onto a file, "replace" is what a real edit would ask for; the session is cancelled before anything is sent.
        let behavior = if item.is_dir() { "fail" } else { "replace" };
        let body = json!({ "item": { "@microsoft.graph.conflictBehavior": behavior } });
        match self.send(Method::POST, &target, |r| r.json(&body)).await {
            Ok(resp) => {
                let session: UploadSession = decode(resp).await?;
                let url = session.upload_url.clone();
                let _ = self.send_plain(&url, crate::client::RequestKind::Upload, |c| c.delete(&url)).await;
                Ok(true)
            }
            Err(GraphError::Http { status: 403, .. }) | Err(GraphError::NotFound(_)) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Rename and/or move an item.
    pub async fn move_item(&self, drive_id: &str, item_id: &str, new_parent_id: Option<&str>, new_name: Option<&str>) -> Result<DriveItem> {
        let mut body = serde_json::Map::new();
        if let Some(p) = new_parent_id {
            body.insert("parentReference".into(), json!({ "id": p }));
        }
        if let Some(n) = new_name {
            body.insert("name".into(), json!(n));
        }
        let body = serde_json::Value::Object(body);
        let resp = self.send(Method::PATCH, &format!("/drives/{drive_id}/items/{item_id}"), |r| r.json(&body)).await?;
        decode(resp).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_paths() {
        assert_eq!(enc_path("runs/a b/#1%.txt"), "runs/a%20b/%231%25.txt");
    }
}
