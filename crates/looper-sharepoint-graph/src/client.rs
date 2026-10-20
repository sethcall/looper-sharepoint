//! Thin Microsoft Graph client. Every request goes through [`GraphClient::send`], which paces requests below the
//! per-user limit, applies the global throttle gate, refreshes the token once on 401, retries transient failures, and
//! classifies errors. See `specs/m1-design.md` §4 and `docs/sharepoint/throttling.md`.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use reqwest::{Method, RequestBuilder, Response, StatusCode};
use serde::Deserialize;
use looper_sharepoint_auth::{AuthError, TokenProvider};

pub const GRAPH: &str = "https://graph.microsoft.com/v1.0";
const MAX_ATTEMPTS: u32 = 6;
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// SharePoint allows 3,000 requests per user per 5 minutes (10/s); stay under it.
pub const DEFAULT_REQUESTS_PER_SECOND: u32 = 8;

#[derive(Debug, thiserror::Error)]
pub enum GraphError {
    /// Gave up after retrying 429/503/504/network errors.
    #[error("still failing after {MAX_ATTEMPTS} attempts: {0}")]
    Retryable(String),
    #[error(transparent)]
    Auth(#[from] AuthError),
    /// 410 Gone on delta: the delta token is no longer valid; re-enumerate.
    #[error("delta token expired; full resync required")]
    ResyncRequired,
    #[error("not found: {0}")]
    NotFound(String),
    #[error("{status} {code}: {message}")]
    Http { status: u16, code: String, message: String },
    #[error("unexpected response: {0}")]
    Decode(String),
}

impl GraphError {
    pub fn is_auth(&self) -> bool {
        matches!(self, GraphError::Auth(_))
    }

    /// 412: an `If-Match` precondition failed (the item changed in SharePoint since we last saw it).
    pub fn is_precondition_failed(&self) -> bool {
        matches!(self, GraphError::Http { status: 412, .. })
    }

    /// 403: the signed-in user has no access to this site or item.
    pub fn is_forbidden(&self) -> bool {
        matches!(self, GraphError::Http { status: 403, .. })
    }

    /// 409: name already taken (with `conflictBehavior = fail`) or another conflict.
    pub fn is_conflict(&self) -> bool {
        matches!(self, GraphError::Http { status: 409, .. })
    }
}

pub type Result<T> = std::result::Result<T, GraphError>;

#[derive(Deserialize)]
struct ErrorBody {
    error: ErrorDetail,
}
#[derive(Deserialize)]
struct ErrorDetail {
    #[serde(default)]
    code: String,
    #[serde(default)]
    message: String,
}

#[derive(Clone)]
pub struct GraphClient {
    /// Authenticated requests to graph.microsoft.com. Does not follow redirects (download redirects go to
    /// pre-authenticated URLs that must not receive our bearer token).
    pub(crate) http: reqwest::Client,
    /// Unauthenticated requests to pre-authenticated URLs (download redirects, upload session chunks).
    pub(crate) plain: reqwest::Client,
    tokens: Arc<dyn TokenProvider>,
    pause_until: Arc<Mutex<Option<Instant>>>,
    /// Wall-clock end of the current throttle pause, for status displays.
    throttle: Arc<tokio::sync::watch::Sender<Option<SystemTime>>>,
    pacer: Arc<Pacer>,
    stats: Arc<Mutex<Stats>>,
}

/// Spaces requests at least `interval` apart (a token bucket with no burst).
struct Pacer {
    interval: Duration,
    next: Mutex<tokio::time::Instant>,
}

impl Pacer {
    fn new(per_second: u32) -> Self {
        Self { interval: Duration::from_secs(1) / per_second.max(1), next: Mutex::new(tokio::time::Instant::now()) }
    }

    async fn wait(&self) {
        let slot = {
            let mut next = self.next.lock().unwrap();
            let slot = (*next).max(tokio::time::Instant::now());
            *next = slot + self.interval;
            slot
        };
        tokio::time::sleep_until(slot).await;
    }
}

/// What a request is, for estimating its SharePoint resource-unit (RU) cost. The costs are Microsoft's published
/// ones for Graph (see `docs/sharepoint/throttling.md`): 1 RU to read a single item, call delta with a token, or
/// download; 2 RU for multi-item reads, delta without a token, and any write. SharePoint doesn't report actual usage,
/// so this is an estimate. Pre-authenticated download/upload hops are counted at the cost of the call they belong to,
/// since they're SharePoint requests too (conservative). Permission calls count 5 RU, what they cost when measured.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestKind {
    /// Delta with a token (incremental).
    Delta,
    /// Delta without a token (full enumeration, or `token=latest`).
    DeltaFull,
    /// Single item / site / user reads, and the push endpoint.
    Read,
    /// Multi-item reads: children, site drives, site search.
    List,
    /// File download (the Graph call and its redirect hop).
    Download,
    /// Upload: simple PUT, creating a session, each chunk.
    Upload,
    /// Create folder, rename/move, delete.
    Write,
    /// Reading or changing an item's permissions (list, invite, update, delete).
    Permissions,
}

impl RequestKind {
    pub fn ru(self) -> u64 {
        match self {
            Self::Delta | Self::Read | Self::Download => 1,
            Self::DeltaFull | Self::List | Self::Upload | Self::Write => 2,
            Self::Permissions => 5,
        }
    }

    /// Classify a Graph request by method and path (relative or absolute URL).
    pub fn classify(method: &Method, path: &str) -> Self {
        let p = path.split('?').next().unwrap_or(path);
        let query = path.split_once('?').map(|(_, q)| q).unwrap_or("");
        if p.ends_with("/delta") || p.contains("/delta(") {
            return if query.contains("token=") && !query.contains("token=latest") { Self::Delta } else { Self::DeltaFull };
        }
        if p.ends_with("/permissions") || p.contains("/permissions/") || p.ends_with("/invite") {
            return Self::Permissions;
        }
        match *method {
            Method::GET if p.ends_with("/content") => Self::Download,
            Method::GET if p.ends_with("/children") || p.ends_with("/drives") || p.ends_with("/sites") || p.ends_with("/followedSites") => Self::List,
            Method::GET => Self::Read,
            Method::PUT if p.ends_with("/content") => Self::Upload,
            Method::POST if p.ends_with("/createUploadSession") => Self::Upload,
            _ => Self::Write,
        }
    }
}

/// Requests of one kind.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct KindStats {
    pub requests: u64,
    /// Estimated resource units.
    pub ru: u64,
}

/// Request accounting, for estimating SharePoint resource-unit usage.
#[derive(Debug, Default, Clone)]
pub struct Stats {
    /// Every attempt, including retries and throttled ones (SharePoint counts those too).
    pub requests: u64,
    pub throttled: u64,
    pub retries: u64,
    /// Estimated resource units, total and per kind.
    pub ru: u64,
    pub by_kind: std::collections::BTreeMap<RequestKind, KindStats>,
}

impl GraphClient {
    pub fn new(tokens: Arc<dyn TokenProvider>) -> Self {
        Self::with_rate(tokens, DEFAULT_REQUESTS_PER_SECOND)
    }

    /// A client that sends at most `per_second` requests per second (downloads included).
    pub fn with_rate(tokens: Arc<dyn TokenProvider>, per_second: u32) -> Self {
        Self {
            http: looper_sharepoint_auth::net::http_client()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(20))
                .build()
                .expect("http client"),
            plain: looper_sharepoint_auth::net::http_client()
                .connect_timeout(Duration::from_secs(20))
                .build()
                .expect("http client"),
            tokens,
            pause_until: Arc::new(Mutex::new(None)),
            throttle: Arc::new(tokio::sync::watch::channel(None).0),
            pacer: Arc::new(Pacer::new(per_second)),
            stats: Arc::new(Mutex::new(Stats::default())),
        }
    }

    pub fn stats(&self) -> Stats {
        self.stats.lock().unwrap().clone()
    }

    /// When the current throttle pause ends, if one is in effect.
    pub fn throttled_until(&self) -> Option<SystemTime> {
        let until = *self.pause_until.lock().unwrap();
        until.filter(|t| *t > Instant::now()).and(*self.throttle.borrow())
    }

    /// Notified whenever a throttle pause starts or is extended.
    pub fn watch_throttle(&self) -> tokio::sync::watch::Receiver<Option<SystemTime>> {
        self.throttle.subscribe()
    }

    pub(crate) fn url(path: &str) -> String {
        if path.starts_with("https://") { path.to_string() } else { format!("{GRAPH}{path}") }
    }

    async fn wait_gate(&self) {
        let until = *self.pause_until.lock().unwrap();
        if let Some(t) = until {
            let now = Instant::now();
            if t > now {
                tokio::time::sleep(t - now).await;
            }
        }
    }

    fn pause_all(&self, d: Duration) {
        let mut g = self.pause_until.lock().unwrap();
        let t = Instant::now() + d;
        if g.is_none_or(|cur| cur < t) {
            *g = Some(t);
            self.throttle.send_replace(Some(SystemTime::now() + d));
        }
    }

    /// Send an authenticated request built by `build` (called once per attempt).
    pub async fn send(&self, method: Method, path: &str, build: impl Fn(RequestBuilder) -> RequestBuilder) -> Result<Response> {
        let url = Self::url(path);
        let kind = RequestKind::classify(&method, path);
        self.send_with(&url, kind, |c, token| build(c.http.request(method.clone(), &url).bearer_auth(token))).await
    }

    /// Unauthenticated request to a pre-authenticated URL, with the same throttle/retry handling.
    pub(crate) async fn send_plain(&self, url: &str, kind: RequestKind, build: impl Fn(&reqwest::Client) -> RequestBuilder) -> Result<Response> {
        self.send_with(url, kind, |c, _| build(&c.plain)).await
    }

    async fn send_with(&self, what: &str, kind: RequestKind, build: impl Fn(&Self, &str) -> RequestBuilder) -> Result<Response> {
        let mut refreshed = false;
        let mut last_err = String::new();
        for attempt in 0..MAX_ATTEMPTS {
            self.wait_gate().await;
            self.pacer.wait().await;
            let token = self.tokens.access_token().await?;
            {
                let mut st = self.stats.lock().unwrap();
                st.requests += 1;
                st.ru += kind.ru();
                let k = st.by_kind.entry(kind).or_default();
                k.requests += 1;
                k.ru += kind.ru();
                if attempt > 0 {
                    st.retries += 1;
                }
            }
            let resp = match build(self, &token).send().await {
                Ok(r) => r,
                Err(e) if e.is_connect() || e.is_timeout() || e.is_request() => {
                    last_err = format!("{what}: {e}");
                    let wait = backoff(attempt);
                    tracing::warn!(attempt, wait_ms = wait.as_millis() as u64, kind = ?kind, "network error; retrying: {e}");
                    tokio::time::sleep(wait).await;
                    continue;
                }
                Err(e) => return Err(GraphError::Decode(format!("{what}: {e}"))),
            };
            let status = resp.status();
            if status.is_success() || status.is_redirection() {
                return Ok(resp);
            }
            match status {
                StatusCode::UNAUTHORIZED if !refreshed => {
                    refreshed = true;
                    self.tokens.refresh_now().await?;
                    continue;
                }
                StatusCode::TOO_MANY_REQUESTS | StatusCode::SERVICE_UNAVAILABLE | StatusCode::GATEWAY_TIMEOUT
                | StatusCode::BAD_GATEWAY => {
                    let wait = retry_after(&resp).unwrap_or_else(|| backoff(attempt));
                    if status == StatusCode::TOO_MANY_REQUESTS || status == StatusCode::SERVICE_UNAVAILABLE {
                        self.stats.lock().unwrap().throttled += 1;
                    }
                    tracing::warn!(%status, wait_ms = wait.as_millis() as u64, "throttled/unavailable; pausing all requests");
                    self.pause_all(wait);
                    last_err = format!("{what}: {status}");
                    continue;
                }
                StatusCode::GONE => return Err(GraphError::ResyncRequired),
                _ => {}
            }
            let body = resp.text().await.unwrap_or_default();
            let (code, message) = serde_json::from_str::<ErrorBody>(&body)
                .map(|e| (e.error.code, e.error.message))
                .unwrap_or_else(|_| (String::new(), body.chars().take(300).collect()));
            if code == "resyncRequired" {
                return Err(GraphError::ResyncRequired);
            }
            if status == StatusCode::NOT_FOUND {
                return Err(GraphError::NotFound(format!("{what}: {message}")));
            }
            if status == StatusCode::UNAUTHORIZED {
                return Err(GraphError::Auth(AuthError::ReauthRequired(message)));
            }
            return Err(GraphError::Http { status: status.as_u16(), code, message });
        }
        Err(GraphError::Retryable(last_err))
    }

    pub async fn get_json<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T> {
        let resp = self.send(Method::GET, path, |r| r).await?;
        decode(resp).await
    }
}

pub(crate) async fn decode<T: serde::de::DeserializeOwned>(resp: Response) -> Result<T> {
    let url = resp.url().to_string();
    let bytes = resp.bytes().await.map_err(|e| GraphError::Decode(format!("{url}: {e}")))?;
    serde_json::from_slice(&bytes).map_err(|e| GraphError::Decode(format!("{url}: {e}")))
}

fn retry_after(resp: &Response) -> Option<Duration> {
    let v = resp.headers().get(reqwest::header::RETRY_AFTER)?.to_str().ok()?;
    v.trim().parse::<u64>().ok().map(Duration::from_secs)
}

/// Exponential backoff with jitter: ~1s, 2s, 4s… capped at 60s.
fn backoff(attempt: u32) -> Duration {
    let base = Duration::from_secs(1u64 << attempt.min(6)).min(MAX_BACKOFF);
    let nanos = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
    let jitter = Duration::from_millis((nanos % 1000) as u64 * base.as_millis().min(1000) as u64 / 1000);
    (base + jitter).min(MAX_BACKOFF)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn pacer_spaces_requests() {
        let p = Pacer::new(8);
        let start = tokio::time::Instant::now();
        for _ in 0..17 {
            p.wait().await;
        }
        // 17 requests at 8/s: the first is immediate, the 17th waits 16 × 125 ms.
        let took = start.elapsed();
        assert!(took >= Duration::from_millis(1990) && took < Duration::from_millis(2100), "{took:?}");
    }

    #[test]
    fn backoff_grows_and_caps() {
        assert!(backoff(0) >= Duration::from_secs(1) && backoff(0) < Duration::from_secs(3));
        assert!(backoff(3) >= Duration::from_secs(8));
        assert!(backoff(20) <= MAX_BACKOFF);
    }
}

#[cfg(test)]
mod classify_tests {
    use super::RequestKind as K;
    use reqwest::Method;

    #[test]
    fn classifies_requests() {
        let c = K::classify;
        assert_eq!(c(&Method::GET, "/drives/d/root/delta?$select=id"), K::DeltaFull);
        assert_eq!(c(&Method::GET, "https://graph.microsoft.com/v1.0/drives/d/root/delta?token=abc"), K::Delta);
        assert_eq!(c(&Method::GET, "/drives/d/root/delta?token=latest"), K::DeltaFull);
        assert_eq!(c(&Method::GET, "/drives/d/items/i/content"), K::Download);
        assert_eq!(c(&Method::GET, "/drives/d/items/i/children"), K::List);
        assert_eq!(c(&Method::GET, "/drives/d/root/subscriptions/socketIo"), K::Read);
        assert_eq!(c(&Method::POST, "/drives/d/items/i/createUploadSession"), K::Upload);
        assert_eq!(c(&Method::PUT, "/drives/d/items/p:/a.txt:/content?x=1"), K::Upload);
        assert_eq!(c(&Method::PATCH, "/drives/d/items/i"), K::Write);
        assert_eq!(c(&Method::DELETE, "/drives/d/items/i"), K::Write);
        assert_eq!(c(&Method::GET, "/drives/d/items/i/permissions"), K::Permissions);
        assert_eq!(c(&Method::DELETE, "/drives/d/items/i/permissions/p"), K::Permissions);
        assert_eq!(c(&Method::POST, "/drives/d/items/i/invite"), K::Permissions);
        assert_eq!(K::Delta.ru() + K::Upload.ru(), 3);
    }
}
