use serde::Deserialize;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DriveItem {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub e_tag: Option<String>,
    #[serde(default)]
    pub c_tag: Option<String>,
    #[serde(default)]
    pub size: Option<i64>,
    #[serde(default)]
    pub parent_reference: Option<ItemReference>,
    #[serde(default)]
    pub file: Option<FileFacet>,
    #[serde(default)]
    pub folder: Option<serde_json::Value>,
    #[serde(default)]
    pub root: Option<serde_json::Value>,
    #[serde(default)]
    pub deleted: Option<serde_json::Value>,
    #[serde(default)]
    pub file_system_info: Option<FileSystemInfo>,
    #[serde(default)]
    pub last_modified_date_time: Option<String>,
    #[serde(default)]
    pub web_url: Option<String>,
    /// SharePoint's own ids for the item (only when selected), for links into SharePoint's pages.
    #[serde(default)]
    pub sharepoint_ids: Option<SharepointIds>,
}

impl DriveItem {
    pub fn is_deleted(&self) -> bool {
        self.deleted.is_some()
    }
    pub fn is_root(&self) -> bool {
        self.root.is_some()
    }
    pub fn is_dir(&self) -> bool {
        self.folder.is_some() || self.root.is_some()
    }
    pub fn parent_id(&self) -> Option<&str> {
        self.parent_reference.as_ref().and_then(|p| p.id.as_deref())
    }
    pub fn quickxor(&self) -> Option<&str> {
        self.file.as_ref()?.hashes.as_ref()?.quick_xor_hash.as_deref()
    }
    /// Client-visible modification time (what the user's app set), falling back to the server's.
    pub fn mtime(&self) -> Option<&str> {
        self.file_system_info
            .as_ref()
            .and_then(|f| f.last_modified_date_time.as_deref())
            .or(self.last_modified_date_time.as_deref())
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ItemReference {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub drive_id: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileFacet {
    #[serde(default)]
    pub hashes: Option<Hashes>,
    #[serde(default)]
    pub mime_type: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Hashes {
    #[serde(default)]
    pub quick_xor_hash: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileSystemInfo {
    #[serde(default)]
    pub last_modified_date_time: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DeltaPage {
    pub value: Vec<DriveItem>,
    #[serde(rename = "@odata.nextLink", default)]
    pub next_link: Option<String>,
    #[serde(rename = "@odata.deltaLink", default)]
    pub delta_link: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Site {
    pub id: String,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub web_url: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Drive {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub drive_type: Option<String>,
    #[serde(default)]
    pub web_url: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct User {
    #[serde(default)]
    pub user_principal_name: Option<String>,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub mail: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Collection<T> {
    pub value: Vec<T>,
    #[serde(rename = "@odata.nextLink", default)]
    pub next_link: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UploadSession {
    pub upload_url: String,
}

/// Response of `GET …/root/subscriptions/socketIo`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SocketIoSubscription {
    #[serde(default)]
    pub id: Option<String>,
    pub notification_url: String,
    #[serde(default)]
    pub expiration_date_time: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SharepointIds {
    #[serde(default)]
    pub list_id: Option<String>,
    #[serde(default)]
    pub list_item_id: Option<String>,
    #[serde(default)]
    pub site_url: Option<String>,
}

/// One entry of an item's permissions (`GET …/permissions`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Permission {
    pub id: String,
    #[serde(default)]
    pub roles: Vec<String>,
    #[serde(default)]
    pub granted_to_v2: Option<SharePointIdentitySet>,
    /// Who a sharing link was given to (links only).
    #[serde(default)]
    pub granted_to_identities_v2: Vec<SharePointIdentitySet>,
    #[serde(default)]
    pub link: Option<SharingLink>,
    #[serde(default)]
    pub invitation: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SharePointIdentitySet {
    #[serde(default)]
    pub user: Option<Identity>,
    #[serde(default)]
    pub group: Option<Identity>,
    #[serde(default)]
    pub site_user: Option<SiteIdentity>,
    #[serde(default)]
    pub site_group: Option<SiteIdentity>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Identity {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
}

/// A SharePoint site user or site group (`loginName` is e.g. `i:0#.f|membership|ann@contoso.com`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SiteIdentity {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub login_name: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SharingLink {
    /// `anonymous`, `organization` or `users`.
    #[serde(default)]
    pub scope: Option<String>,
    /// `view`, `edit`, `embed`…
    #[serde(rename = "type", default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub web_url: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VerifiedDomain {
    pub name: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Organization {
    #[serde(default)]
    pub verified_domains: Vec<VerifiedDomain>,
}
