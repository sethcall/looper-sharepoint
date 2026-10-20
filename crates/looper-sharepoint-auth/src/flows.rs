use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::{URL_SAFE_NO_PAD, STANDARD_NO_PAD};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use crate::{AuthError, Authority};

const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Debug, Clone)]
pub struct TokenResponse {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: Instant,
    /// `preferred_username` from the ID token, if one was returned.
    pub username: Option<String>,
    /// `tid` (tenant GUID) from the ID token.
    pub tenant_id: Option<String>,
}

#[derive(Deserialize)]
struct RawToken {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: u64,
    id_token: Option<String>,
}

#[derive(Deserialize)]
struct RawError {
    error: String,
    #[serde(default)]
    error_description: String,
}

#[derive(Deserialize)]
struct IdClaims {
    preferred_username: Option<String>,
    tid: Option<String>,
}

fn http() -> reqwest::Client {
    crate::net::http_client().build()
        .expect("http client")
}

async fn token_request(authority: &Authority, form: &[(&str, &str)]) -> Result<TokenResponse, AuthError> {
    let resp = http().post(authority.token_url()).form(form).send().await?;
    let status = resp.status();
    let body = resp.text().await?;
    if !status.is_success() {
        return Err(match serde_json::from_str::<RawError>(&body) {
            Ok(e) => AuthError::Oauth { code: e.error, description: e.error_description },
            Err(_) => AuthError::Other(format!("token endpoint returned {status}: {body}")),
        });
    }
    let raw: RawToken = serde_json::from_str(&body).map_err(|e| AuthError::Other(format!("token response: {e}")))?;
    let claims = raw.id_token.as_deref().and_then(decode_claims);
    Ok(TokenResponse {
        access_token: raw.access_token,
        refresh_token: raw.refresh_token,
        expires_at: Instant::now() + Duration::from_secs(raw.expires_in),
        username: claims.as_ref().and_then(|c| c.preferred_username.clone()),
        tenant_id: claims.and_then(|c| c.tid),
    })
}

/// Reads claims from the ID token for display and keychain keys only. Not a security decision, so the
/// signature is not verified (the token came straight from the token endpoint over TLS).
fn decode_claims(jwt: &str) -> Option<IdClaims> {
    let payload = jwt.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload).or_else(|_| STANDARD_NO_PAD.decode(payload)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

pub async fn refresh(authority: &Authority, refresh_token: &str) -> Result<TokenResponse, AuthError> {
    let scope = authority.scope_string();
    token_request(
        authority,
        &[
            ("client_id", &authority.client_id),
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("scope", &scope),
        ],
    )
    .await
}

/// Resource owner password grant. Test fixtures only: fails for any account with MFA.
pub async fn password_login(authority: &Authority, username: &str, password: &str) -> Result<TokenResponse, AuthError> {
    let scope = authority.scope_string();
    token_request(
        authority,
        &[
            ("client_id", &authority.client_id),
            ("grant_type", "password"),
            ("username", username),
            ("password", password),
            ("scope", &scope),
        ],
    )
    .await
}

fn random_url_safe(n: usize) -> String {
    let mut buf = vec![0u8; n];
    getrandom::fill(&mut buf).expect("OS random source");
    URL_SAFE_NO_PAD.encode(buf)
}

/// Interactive sign-in: opens the browser (via `open_url`) and waits for the redirect to a local listener.
pub async fn pkce_login(authority: &Authority, open_url: impl FnOnce(&str)) -> Result<TokenResponse, AuthError> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    // Browsers may resolve `localhost` to ::1 first; listen there too when possible.
    let listener6 = TcpListener::bind(("::1", port)).await.ok();
    let redirect_uri = format!("http://localhost:{port}");

    let verifier = random_url_safe(48);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let state = random_url_safe(16);

    let mut url = url::Url::parse(&format!(
        "https://login.microsoftonline.com/{}/oauth2/v2.0/authorize",
        authority.tenant
    ))
    .map_err(|e| AuthError::Other(e.to_string()))?;
    url.query_pairs_mut()
        .append_pair("client_id", &authority.client_id)
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", &redirect_uri)
        .append_pair("response_mode", "query")
        .append_pair("scope", &authority.scope_string())
        .append_pair("state", &state)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("prompt", "select_account");
    open_url(url.as_str());

    let code = tokio::time::timeout(LOGIN_TIMEOUT, wait_for_code(&listener, listener6.as_ref(), &state))
        .await
        .map_err(|_| AuthError::Timeout)??;

    token_request(
        authority,
        &[
            ("client_id", &authority.client_id),
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("redirect_uri", &redirect_uri),
            ("code_verifier", &verifier),
            ("scope", &authority.scope_string()),
        ],
    )
    .await
}

async fn wait_for_code(v4: &TcpListener, v6: Option<&TcpListener>, state: &str) -> Result<String, AuthError> {
    loop {
        let (mut sock, _) = match v6 {
            Some(v6) => tokio::select! { r = v4.accept() => r?, r = v6.accept() => r? },
            None => v4.accept().await?,
        };
        let mut buf = vec![0u8; 16 * 1024];
        let mut len = 0;
        while len < buf.len() {
            let n = sock.read(&mut buf[len..]).await?;
            if n == 0 {
                break;
            }
            len += n;
            if buf[..len].windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        let req = String::from_utf8_lossy(&buf[..len]);
        let target = req.lines().next().and_then(|l| l.split_whitespace().nth(1)).unwrap_or("/");
        let Ok(url) = url::Url::parse(&format!("http://localhost{target}")) else { continue };
        let params: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        if params.is_empty() {
            // favicon or similar
            let _ = sock.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n").await;
            continue;
        }
        let name = html_escape(&crate::net::current().app_name);
        let signed_in = format!("Signed in to {name}. You can close this tab.");
        let (status, body, result) = if params.get("state").map(String::as_str) != Some(state) {
            ("400 Bad Request", "Sign-in failed: state mismatch. You can close this tab.", Err(AuthError::Other("state mismatch".into())))
        } else if let Some(code) = params.get("code") {
            ("200 OK", signed_in.as_str(), Ok(code.clone()))
        } else {
            let err = params.get("error").cloned().unwrap_or_else(|| "unknown_error".into());
            let desc = params.get("error_description").cloned().unwrap_or_default();
            ("400 Bad Request", "Sign-in failed. See the terminal for details. You can close this tab.", Err(AuthError::Oauth { code: err, description: desc }))
        };
        let html = format!("<!doctype html><meta charset=utf-8><title>{name}</title><p style=\"font:16px system-ui;margin:3em\">{body}</p>");
        let _ = sock
            .write_all(
                format!("HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{html}", html.len())
                    .as_bytes(),
            )
            .await;
        return result;
    }
}

/// The app name comes from config, so escape it before putting it in the page.
fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

#[derive(Debug, Clone, Deserialize)]
pub struct DeviceCodeInfo {
    pub user_code: String,
    pub device_code: String,
    pub verification_uri: String,
    pub expires_in: u64,
    pub interval: u64,
    pub message: String,
}

/// Device code sign-in: `show` is called once with the code and URL for the user.
pub async fn device_code_login(authority: &Authority, show: impl FnOnce(&DeviceCodeInfo)) -> Result<TokenResponse, AuthError> {
    let resp = http()
        .post(format!("https://login.microsoftonline.com/{}/oauth2/v2.0/devicecode", authority.tenant))
        .form(&[("client_id", authority.client_id.as_str()), ("scope", &authority.scope_string())])
        .send()
        .await?;
    if !resp.status().is_success() {
        let e: RawError = resp.json().await?;
        return Err(AuthError::Oauth { code: e.error, description: e.error_description });
    }
    let info: DeviceCodeInfo = resp.json().await?;
    show(&info);

    let deadline = Instant::now() + Duration::from_secs(info.expires_in);
    let mut interval = Duration::from_secs(info.interval.max(1));
    while Instant::now() < deadline {
        tokio::time::sleep(interval).await;
        match token_request(
            authority,
            &[
                ("client_id", &authority.client_id),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("device_code", &info.device_code),
            ],
        )
        .await
        {
            Err(AuthError::Oauth { code, .. }) if code == "authorization_pending" => {}
            Err(AuthError::Oauth { code, .. }) if code == "slow_down" => interval += Duration::from_secs(5),
            r => return r,
        }
    }
    Err(AuthError::Timeout)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_id_token_claims() {
        let payload = URL_SAFE_NO_PAD.encode(br#"{"preferred_username":"a@b.com","tid":"t-1"}"#);
        let c = decode_claims(&format!("x.{payload}.y")).unwrap();
        assert_eq!(c.preferred_username.as_deref(), Some("a@b.com"));
        assert_eq!(c.tid.as_deref(), Some("t-1"));
    }
}
