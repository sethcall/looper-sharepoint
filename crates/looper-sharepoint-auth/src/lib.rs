//! Entra ID sign-in for a public (desktop) client. See `specs/m1-design.md` §5.
//!
//! - Interactive: auth code + PKCE with a loopback redirect (`http://localhost:<port>`).
//! - Fallback: device code.
//! - The refresh token lives in the OS keychain; access tokens only in memory.
//! - Tests only: resource owner password grant, for the MFA-less QA account.

mod flows;
pub mod net;
mod store;

pub use flows::{DeviceCodeInfo, TokenResponse, device_code_login, password_login, pkce_login, refresh};
pub use store::{AccountRecord, TokenStore};

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::sync::Mutex;

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("not signed in; run `looper-sharepoint login`")]
    NotSignedIn,
    #[error("sign-in expired or was revoked ({0}); run `looper-sharepoint login`")]
    ReauthRequired(String),
    #[error("token endpoint error {code}: {description}")]
    Oauth { code: String, description: String },
    #[error("sign-in timed out")]
    Timeout,
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("keychain: {0}")]
    Keychain(#[from] keyring::Error),
    #[error("{0}")]
    Other(String),
}

impl AuthError {
    /// The user must sign in again (no account, or the refresh token was rejected). Network errors are not this.
    pub fn needs_sign_in(&self) -> bool {
        matches!(self, AuthError::NotSignedIn | AuthError::ReauthRequired(_))
    }
}

/// Where and as whom to sign in.
#[derive(Debug, Clone)]
pub struct Authority {
    /// Tenant GUID or domain.
    pub tenant: String,
    pub client_id: String,
    pub scopes: Vec<String>,
}

impl Authority {
    pub fn token_url(&self) -> String {
        format!("https://login.microsoftonline.com/{}/oauth2/v2.0/token", self.tenant)
    }

    pub fn scope_string(&self) -> String {
        self.scopes
            .iter()
            .map(|s| match s.as_str() {
                // OIDC scopes and offline_access are not resource-qualified.
                "openid" | "profile" | "offline_access" | "email" => s.clone(),
                s if s.starts_with("https://") => s.to_string(),
                s => format!("https://graph.microsoft.com/{s}"),
            })
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// Supplies bearer tokens to the Graph client.
#[async_trait]
pub trait TokenProvider: Send + Sync {
    async fn access_token(&self) -> Result<String, AuthError>;
    /// The signed-in user's name, if the provider knows it without a network call.
    fn username(&self) -> Option<String> {
        None
    }
    /// Drop any cached access token (called after a 401) and get a fresh one.
    async fn refresh_now(&self) -> Result<String, AuthError>;
}

struct Cached {
    token: String,
    expires: Instant,
}

/// Uses (and rotates) the refresh token stored in the keychain.
///
/// The signed-in account is looked up on every refresh, so a long-running host started before sign-in (or across a
/// sign-out and sign-in) picks up the current account without being rebuilt.
pub struct RefreshingProvider {
    authority: Authority,
    store: TokenStore,
    cached: Mutex<Option<Cached>>,
}

impl RefreshingProvider {
    pub fn new(authority: Authority, store: TokenStore) -> Arc<Self> {
        Arc::new(Self { authority, store, cached: Mutex::new(None) })
    }

    /// The account currently signed in for this tenant and app, if any.
    pub fn account(&self) -> Result<Option<AccountRecord>, AuthError> {
        self.store.account(&self.authority.tenant, &self.authority.client_id)
    }

    /// Drop the cached access token (e.g. after signing in as someone else).
    pub async fn clear(&self) {
        *self.cached.lock().await = None;
    }

    async fn fetch(&self) -> Result<Cached, AuthError> {
        let account = self.account()?.ok_or(AuthError::NotSignedIn)?;
        let rt = self.store.refresh_token(&account)?.ok_or(AuthError::NotSignedIn)?;
        let resp = match refresh(&self.authority, &rt).await {
            Err(AuthError::Oauth { code, description }) if code == "invalid_grant" || code == "interaction_required" => {
                return Err(AuthError::ReauthRequired(description.lines().next().unwrap_or("").to_string()));
            }
            r => r?,
        };
        if let Some(new_rt) = &resp.refresh_token {
            self.store.save_refresh_token(&account, new_rt)?;
        }
        Ok(Cached { token: resp.access_token, expires: resp.expires_at })
    }
}

#[async_trait]
impl TokenProvider for RefreshingProvider {
    async fn access_token(&self) -> Result<String, AuthError> {
        let mut guard = self.cached.lock().await;
        if let Some(c) = guard.as_ref()
            && c.expires > Instant::now() + Duration::from_secs(120) {
                return Ok(c.token.clone());
            }
        let c = self.fetch().await?;
        let t = c.token.clone();
        *guard = Some(c);
        Ok(t)
    }

    fn username(&self) -> Option<String> {
        self.account().ok().flatten().map(|a| a.username)
    }

    async fn refresh_now(&self) -> Result<String, AuthError> {
        let mut guard = self.cached.lock().await;
        let c = self.fetch().await?;
        let t = c.token.clone();
        *guard = Some(c);
        Ok(t)
    }
}

/// Test-only provider using the password grant. Works only for accounts without MFA.
pub struct PasswordProvider {
    authority: Authority,
    username: String,
    password: String,
    cached: Mutex<Option<Cached>>,
}

impl PasswordProvider {
    pub fn new(authority: Authority, username: String, password: String) -> Arc<Self> {
        Arc::new(Self { authority, username, password, cached: Mutex::new(None) })
    }
}

#[async_trait]
impl TokenProvider for PasswordProvider {
    async fn access_token(&self) -> Result<String, AuthError> {
        let mut guard = self.cached.lock().await;
        if let Some(c) = guard.as_ref()
            && c.expires > Instant::now() + Duration::from_secs(120) {
                return Ok(c.token.clone());
            }
        let r = password_login(&self.authority, &self.username, &self.password).await?;
        *guard = Some(Cached { token: r.access_token.clone(), expires: r.expires_at });
        Ok(r.access_token)
    }

    fn username(&self) -> Option<String> {
        Some(self.username.clone())
    }

    async fn refresh_now(&self) -> Result<String, AuthError> {
        *self.cached.lock().await = None;
        self.access_token().await
    }
}

/// A fixed token (for tests or `--token`-style debugging).
pub struct StaticToken(pub String);

#[async_trait]
impl TokenProvider for StaticToken {
    async fn access_token(&self) -> Result<String, AuthError> {
        Ok(self.0.clone())
    }
    async fn refresh_now(&self) -> Result<String, AuthError> {
        Ok(self.0.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scopes_are_qualified() {
        let a = Authority {
            tenant: "t".into(),
            client_id: "c".into(),
            scopes: vec!["Files.ReadWrite.All".into(), "offline_access".into(), "openid".into()],
        };
        assert_eq!(
            a.scope_string(),
            "https://graph.microsoft.com/Files.ReadWrite.All offline_access openid"
        );
    }
}
