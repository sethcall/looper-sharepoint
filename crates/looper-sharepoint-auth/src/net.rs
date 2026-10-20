//! Settings every connection shares: the product name (shown after sign-in and sent in the User-Agent) and the
//! minimum TLS version. Hosts set them once from config with [`configure`]; until then the defaults apply.

use std::sync::RwLock;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum MinTls {
    Tls12,
    #[default]
    Tls13,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientIdentity {
    /// Product name shown to the user, e.g. "Contoso SharePoint Sync".
    pub app_name: String,
    /// Organisation in the User-Agent (`ISV|<publisher>|<app>/<version>`).
    pub publisher: String,
    pub min_tls: MinTls,
}

pub const DEFAULT_NAME: &str = "looper-sharepoint";

impl Default for ClientIdentity {
    fn default() -> Self {
        Self { app_name: DEFAULT_NAME.into(), publisher: DEFAULT_NAME.into(), min_tls: MinTls::default() }
    }
}

static CURRENT: RwLock<Option<ClientIdentity>> = RwLock::new(None);

/// Set the identity for connections made from now on.
pub fn configure(id: ClientIdentity) {
    *CURRENT.write().unwrap() = Some(id);
}

pub fn current() -> ClientIdentity {
    CURRENT.read().unwrap().clone().unwrap_or_default()
}

/// `ISV|Publisher|AppName/version`, the form Microsoft asks apps to identify their traffic with. Characters that
/// aren't allowed in a header token are dropped (spaces included), so "Contoso SharePoint Sync" becomes
/// "ContosoSharePointSync".
pub fn user_agent() -> String {
    let id = current();
    format!("ISV|{}|{}/{}", token(&id.publisher), token(&id.app_name), env!("CARGO_PKG_VERSION"))
}

fn token(s: &str) -> String {
    let t: String = s.chars().filter(|c| c.is_ascii_alphanumeric() || "-._".contains(*c)).collect();
    if t.is_empty() { DEFAULT_NAME.into() } else { t }
}

/// A `reqwest` builder with the User-Agent and minimum TLS version applied.
pub fn http_client() -> reqwest::ClientBuilder {
    let min = match current().min_tls {
        MinTls::Tls12 => reqwest::tls::Version::TLS_1_2,
        MinTls::Tls13 => reqwest::tls::Version::TLS_1_3,
    };
    reqwest::Client::builder().user_agent(user_agent()).min_tls_version(min)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_agent_tokens() {
        assert_eq!(token("Contoso SharePoint Sync"), "ContosoSharePointSync");
        assert_eq!(token("Contoso"), "Contoso");
        assert_eq!(token("  "), DEFAULT_NAME);
    }
}
