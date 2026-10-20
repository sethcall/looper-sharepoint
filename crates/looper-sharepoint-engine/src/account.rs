//! Signing in and out for a config, shared by every host. Hosts supply only the UI: opening a URL or showing a
//! device code.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use looper_sharepoint_auth::{
    AccountRecord, Authority, DeviceCodeInfo, RefreshingProvider, StaticToken, TokenStore, device_code_login, pkce_login,
};
use looper_sharepoint_core::config::{AuthFlow, Config};
use looper_sharepoint_graph::GraphClient;

/// What the host must show the user during sign-in.
pub enum SignInPrompt<'a> {
    /// Open this URL in a browser (PKCE). The user signs in there; the redirect comes back to a loopback listener.
    OpenUrl(&'a str),
    /// Show this code and URL (device code flow, for machines without a browser).
    DeviceCode(&'a DeviceCodeInfo),
}

/// The sign-in authority for `cfg`. Also applies the config's product name and TLS minimum to every connection
/// made from now on ([`configure_client`]), since every host goes through here before connecting.
pub fn authority(cfg: &Config) -> Authority {
    configure_client(cfg);
    Authority { tenant: cfg.account.tenant.clone(), client_id: cfg.account.client_id.clone(), scopes: cfg.account.scopes.clone() }
}

/// Apply `account.app_name`, `account.publisher` and `network.min_tls` to all connections.
pub fn configure_client(cfg: &Config) {
    use looper_sharepoint_auth::net;
    net::configure(net::ClientIdentity {
        app_name: cfg.account.app_name.clone(),
        publisher: cfg.account.publisher.clone(),
        min_tls: match cfg.network.min_tls {
            looper_sharepoint_core::config::MinTls::Tls12 => net::MinTls::Tls12,
            looper_sharepoint_core::config::MinTls::Tls13 => net::MinTls::Tls13,
        },
    });
}

/// The keychain-backed token provider for `cfg`. Works before sign-in (requests fail with "not signed in" until then).
pub fn token_provider(cfg: &Config, data_dir: &Path) -> Arc<RefreshingProvider> {
    RefreshingProvider::new(authority(cfg), TokenStore::new(data_dir.to_owned()))
}

/// The account signed in for `cfg`, if any.
pub fn signed_in_account(cfg: &Config, data_dir: &Path) -> Result<Option<AccountRecord>> {
    Ok(TokenStore::new(data_dir.to_owned()).account(&cfg.account.tenant, &cfg.account.client_id)?)
}

/// Interactive sign-in. Stores the refresh token in the OS keychain and returns the account.
///
/// `flow` defaults to the config's `account.auth_flow` when `None`. If a [`crate::Service`] is running in this
/// process, call [`crate::Service::signed_in`] afterwards.
pub async fn sign_in(cfg: &Config, data_dir: &Path, flow: Option<AuthFlow>, prompt: impl Fn(SignInPrompt)) -> Result<AccountRecord> {
    let auth = authority(cfg);
    let resp = match flow.unwrap_or(cfg.account.auth_flow) {
        AuthFlow::DeviceCode => device_code_login(&auth, |info| prompt(SignInPrompt::DeviceCode(info))).await?,
        AuthFlow::Pkce => pkce_login(&auth, |url| prompt(SignInPrompt::OpenUrl(url))).await?,
    };
    let refresh = resp.refresh_token.clone().context("no refresh token returned; is offline_access in account.scopes?")?;
    let username = match resp.username.clone() {
        Some(u) => u,
        None => {
            let g = GraphClient::new(Arc::new(StaticToken(resp.access_token.clone())));
            g.me().await?.user_principal_name.unwrap_or_else(|| "unknown".into())
        }
    };
    let account = AccountRecord {
        tenant: cfg.account.tenant.clone(),
        client_id: cfg.account.client_id.clone(),
        tenant_id: resp.tenant_id.clone().unwrap_or_else(|| cfg.account.tenant.clone()),
        username,
    };
    TokenStore::new(data_dir.to_owned()).sign_in(&account, &refresh)?;
    Ok(account)
}

/// Forget the stored sign-in. Returns the account that was signed out, if any.
pub fn sign_out(cfg: &Config, data_dir: &Path) -> Result<Option<AccountRecord>> {
    let store = TokenStore::new(data_dir.to_owned());
    match store.account(&cfg.account.tenant, &cfg.account.client_id)? {
        Some(a) => {
            store.forget(&a)?;
            Ok(Some(a))
        }
        None => Ok(None),
    }
}
