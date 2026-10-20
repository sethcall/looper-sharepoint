//! Change notifications over Graph's socket.io endpoint (`GET …/root/subscriptions/socketIo`).
//!
//! A minimal, receive-only socket.io client: Engine.IO v4 over a websocket, one namespace, one event name. What we
//! learned about the endpoint (checked on the QA tenant, 2026-10-11) is in `docs/sharepoint/push.md`:
//!
//! - `notificationUrl` is `https://<host>/<namespace>?token=…&applicationId=…`; the path is the socket.io namespace.
//!   The URL is pre-authenticated, so no bearer token is sent.
//! - Websocket transport only (no long-polling, no upgrades). The server pings every 30 s with a 20 s timeout.
//! - Each `notification` event carries a JSON *string* with `{"value": [{subscriptionId, expirationDateTime, …}]}`
//!   and no detail about what changed: it means "run delta".
//! - Notifications are batched: the first change after a quiet period is announced ~30 s later, together with any
//!   further changes in that window.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

/// How long to wait for the Engine.IO and socket.io handshakes.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, thiserror::Error)]
pub enum PushError {
    #[error("connect failed: {0}")]
    Connect(String),
    #[error("protocol error: {0}")]
    Protocol(String),
    /// The server closed the connection (or the namespace).
    #[error("closed by server: {0}")]
    Closed(String),
    #[error("no heartbeat from server for {0:?}")]
    HeartbeatTimeout(Duration),
}

/// One `notification` event. Graph says nothing about *what* changed; treat it as "this drive changed".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Notification {
    pub subscription_id: Option<String>,
    /// When the subscription behind this socket expires (RFC 3339), if Graph said.
    pub expiration: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct NotificationItem {
    #[serde(default)]
    subscription_id: Option<String>,
    #[serde(default)]
    expiration_date_time: Option<String>,
}

#[derive(Deserialize)]
struct NotificationBody {
    #[serde(default)]
    value: Vec<NotificationItem>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct OpenPacket {
    ping_interval: u64,
    ping_timeout: u64,
}

/// An open socket.io connection to a drive's notification endpoint.
pub struct PushSocket {
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
    namespace: String,
    /// The server must send something (normally a ping) at least this often.
    heartbeat: Duration,
    /// When the last frame arrived. The heartbeat deadline hangs off this, so [`PushSocket::next`] is cancel-safe:
    /// dropping its future (e.g. in a `select!`) doesn't restart the timer.
    last_seen: tokio::time::Instant,
}

impl PushSocket {
    /// Connect to a `notificationUrl` and join its namespace.
    pub async fn connect(notification_url: &str) -> Result<Self, PushError> {
        let (ws_url, namespace) = websocket_url(notification_url)?;
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;
        let mut request = ws_url.as_str().into_client_request().map_err(|e| PushError::Connect(e.to_string()))?;
        if let Ok(ua) = looper_sharepoint_auth::net::user_agent().parse() {
            request.headers_mut().insert("User-Agent", ua);
        }
        let connector = tokio_tungstenite::Connector::Rustls(tls_config()?);
        let connect = tokio_tungstenite::connect_async_tls_with_config(request, None, false, Some(connector));
        let (mut ws, _) = tokio::time::timeout(HANDSHAKE_TIMEOUT, connect)
            .await
            .map_err(|_| PushError::Connect("timed out".into()))?
            .map_err(|e| PushError::Connect(e.to_string()))?;

        // Engine.IO open packet: 0{"sid":…,"pingInterval":…,"pingTimeout":…}
        let open = next_text(&mut ws, HANDSHAKE_TIMEOUT).await?;
        let body = open.strip_prefix('0').ok_or_else(|| PushError::Protocol(format!("expected open packet, got {open:?}")))?;
        let open: OpenPacket = serde_json::from_str(body).map_err(|e| PushError::Protocol(format!("open packet: {e}")))?;
        let heartbeat = Duration::from_millis(open.ping_interval + open.ping_timeout);

        // Join the namespace and wait for the acknowledgement (40<ns>,{"sid":…}) or a refusal (44<ns>,{…}).
        send(&mut ws, format!("40{namespace},")).await?;
        let deadline = tokio::time::Instant::now() + HANDSHAKE_TIMEOUT;
        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            let t = next_text(&mut ws, left).await?;
            if t == "2" {
                send(&mut ws, "3".into()).await?;
            } else if t.starts_with(&format!("40{namespace}")) {
                break;
            } else if let Some(err) = t.strip_prefix(&format!("44{namespace},")) {
                return Err(PushError::Connect(format!("namespace refused: {err}")));
            } else {
                return Err(PushError::Protocol(format!("expected namespace ack, got {t:?}")));
            }
        }
        Ok(Self { ws, namespace, heartbeat, last_seen: tokio::time::Instant::now() })
    }

    /// Wait for the next notification. Answers pings; fails when the connection closes or goes quiet. Cancel-safe.
    pub async fn next(&mut self) -> Result<Notification, PushError> {
        loop {
            let left = (self.last_seen + self.heartbeat).saturating_duration_since(tokio::time::Instant::now());
            let t = next_text(&mut self.ws, left).await.map_err(|e| match e {
                PushError::HeartbeatTimeout(_) => PushError::HeartbeatTimeout(self.heartbeat),
                e => e,
            })?;
            self.last_seen = tokio::time::Instant::now();
            match t.as_bytes().first() {
                Some(b'2') => send(&mut self.ws, "3".into()).await?,
                Some(b'1') => return Err(PushError::Closed("engine.io close".into())),
                Some(b'4') => {
                    let Some(rest) = t.strip_prefix('4') else { continue };
                    if let Some(n) = self.socket_io_packet(rest)? {
                        return Ok(n);
                    }
                }
                // 3 (pong), 6 (noop) and anything unknown: just proof of life.
                _ => {}
            }
        }
    }

    /// Close politely. Errors are ignored: the connection is going away anyway.
    pub async fn close(mut self) {
        let _ = send(&mut self.ws, format!("41{},", self.namespace)).await;
        let _ = self.ws.close(None).await;
    }

    /// Handle a socket.io packet (an Engine.IO message, without its leading `4`).
    fn socket_io_packet(&self, p: &str) -> Result<Option<Notification>, PushError> {
        let (kind, rest) = p.split_at(p.len().min(1));
        // Strip "<namespace>," if present.
        let rest = rest.strip_prefix(self.namespace.as_str()).map(|r| r.strip_prefix(',').unwrap_or(r)).unwrap_or(rest);
        match kind {
            "2" => Ok(parse_event(rest)),
            "1" => Err(PushError::Closed("namespace disconnected".into())),
            "4" => Err(PushError::Closed(format!("namespace error: {rest}"))),
            _ => Ok(None),
        }
    }
}

/// TLS for the websocket, matching the HTTP clients: the operating system's trust store (so a corporate root CA
/// works the same as for every other request) and the configured minimum TLS version.
fn tls_config() -> Result<std::sync::Arc<rustls::ClientConfig>, PushError> {
    use looper_sharepoint_auth::net::MinTls;
    use rustls_platform_verifier::BuilderVerifierExt;
    let versions: &[&rustls::SupportedProtocolVersion] = match looper_sharepoint_auth::net::current().min_tls {
        MinTls::Tls13 => &[&rustls::version::TLS13],
        MinTls::Tls12 => &[&rustls::version::TLS13, &rustls::version::TLS12],
    };
    let tls = |e: rustls::Error| PushError::Connect(format!("TLS setup: {e}"));
    let config = rustls::ClientConfig::builder_with_provider(std::sync::Arc::new(rustls::crypto::aws_lc_rs::default_provider()))
        .with_protocol_versions(versions)
        .map_err(tls)?
        .with_platform_verifier()
        .map_err(tls)?
        .with_no_client_auth();
    Ok(std::sync::Arc::new(config))
}

/// `https://host/ns?q` → (`wss://host/socket.io/?q&EIO=4&transport=websocket`, `/ns`). `http` maps to `ws` (tests).
fn websocket_url(notification_url: &str) -> Result<(url::Url, String), PushError> {
    let mut u = url::Url::parse(notification_url).map_err(|e| PushError::Connect(format!("bad notificationUrl: {e}")))?;
    // Older docs describe `…/<ns>/callback?…`; the endpoint is the part before `/callback`.
    let path = u.path().trim_end_matches('/').trim_end_matches("/callback");
    let namespace = if path.is_empty() { "/".to_string() } else { path.to_string() };
    let scheme = if u.scheme() == "http" { "ws" } else { "wss" };
    u.set_scheme(scheme).map_err(|_| PushError::Connect("cannot set websocket scheme".into()))?;
    u.set_path("/socket.io/");
    let query = match u.query() {
        Some(q) if !q.is_empty() => format!("{q}&EIO=4&transport=websocket"),
        _ => "EIO=4&transport=websocket".into(),
    };
    u.set_query(Some(&query));
    // The default namespace is joined as "" in packets ("40,") — use an empty string for it.
    Ok((u, if namespace == "/" { String::new() } else { namespace }))
}

/// `["notification", "<json string>" | {…}]` → a notification; any other event is ignored.
fn parse_event(payload: &str) -> Option<Notification> {
    let v: serde_json::Value = serde_json::from_str(payload).ok()?;
    let arr = v.as_array()?;
    if arr.first()?.as_str()? != "notification" {
        return None;
    }
    let body = match arr.get(1) {
        Some(serde_json::Value::String(s)) => serde_json::from_str::<NotificationBody>(s).ok(),
        Some(obj) => serde_json::from_value::<NotificationBody>(obj.clone()).ok(),
        None => None,
    };
    let item = body.and_then(|b| b.value.into_iter().next());
    Some(Notification {
        subscription_id: item.as_ref().and_then(|i| i.subscription_id.clone()),
        expiration: item.and_then(|i| i.expiration_date_time),
    })
}

async fn send(ws: &mut WebSocketStream<MaybeTlsStream<TcpStream>>, text: String) -> Result<(), PushError> {
    ws.send(Message::text(text)).await.map_err(|e| PushError::Closed(e.to_string()))
}

/// The next text frame, within `wait`. Websocket pings are answered by tungstenite; close frames end the stream.
async fn next_text(ws: &mut WebSocketStream<MaybeTlsStream<TcpStream>>, wait: Duration) -> Result<String, PushError> {
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        let msg = tokio::time::timeout_at(deadline, ws.next()).await.map_err(|_| PushError::HeartbeatTimeout(wait))?;
        match msg {
            None => return Err(PushError::Closed("connection ended".into())),
            Some(Err(e)) => return Err(PushError::Closed(e.to_string())),
            Some(Ok(Message::Text(t))) => return Ok(t.as_str().to_string()),
            Some(Ok(Message::Close(c))) => {
                return Err(PushError::Closed(c.map(|c| format!("{} {}", c.code, c.reason)).unwrap_or_else(|| "close frame".into())));
            }
            Some(Ok(_)) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    #[test]
    fn websocket_url_from_notification_url() {
        let (u, ns) = websocket_url("https://centralus1-2.pushnp.svc.ms/notifications?token=abc&applicationId=x").unwrap();
        assert_eq!(u.as_str(), "wss://centralus1-2.pushnp.svc.ms/socket.io/?token=abc&applicationId=x&EIO=4&transport=websocket");
        assert_eq!(ns, "/notifications");
        let (u, ns) = websocket_url("https://f3hb0mpua.svc.ms/zbaehwg/callback?snthgk=1ff3").unwrap();
        assert_eq!(u.path(), "/socket.io/");
        assert_eq!(ns, "/zbaehwg");
        let (u, ns) = websocket_url("http://127.0.0.1:9/").unwrap();
        assert_eq!(u.scheme(), "ws");
        assert_eq!(ns, "");
    }

    #[test]
    fn parses_graph_notification() {
        // Verbatim shape from the QA tenant (ids shortened).
        let p = r#"["notification","{\"value\":[{\"subscriptionId\":\"c1d4\",\"clientState\":\"system-managed:80\",\"expirationDateTime\":\"2026-10-11T17:57:33.4740000Z\",\"resource\":\"265d\",\"tenantId\":\"d258\",\"siteUrl\":\"/sites/qa\",\"webId\":\"be2a\"}]}"]"#;
        let n = parse_event(p).unwrap();
        assert_eq!(n.subscription_id.as_deref(), Some("c1d4"));
        assert_eq!(n.expiration.as_deref(), Some("2026-10-11T17:57:33.4740000Z"));
        assert_eq!(parse_event(r#"["notification",{"value":[]}]"#), Some(Notification::default()));
        assert_eq!(parse_event(r#"["other",{}]"#), None);
    }

    /// A fake Graph push server: Engine.IO handshake, namespace ack, then runs `script` on the socket.
    async fn fake_server<F, Fut>(script: F) -> String
    where
        F: FnOnce(WebSocketStream<TcpStream>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send,
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
            ws.send(Message::text(r#"0{"sid":"s","upgrades":[],"pingInterval":200,"pingTimeout":100,"maxPayload":1000}"#)).await.unwrap();
            let join = ws.next().await.unwrap().unwrap();
            assert_eq!(join.to_text().unwrap(), "40/notifications,");
            ws.send(Message::text(r#"40/notifications,{"sid":"n"}"#)).await.unwrap();
            script(ws).await;
        });
        format!("http://{addr}/notifications?token=t")
    }

    #[tokio::test]
    async fn receives_notifications_and_answers_pings() {
        let url = fake_server(|mut ws| async move {
            ws.send(Message::text("2")).await.unwrap();
            assert_eq!(ws.next().await.unwrap().unwrap().to_text().unwrap(), "3");
            ws.send(Message::text(r#"42/notifications,["notification","{\"value\":[{\"subscriptionId\":\"a\"}]}"]"#)).await.unwrap();
            ws.send(Message::text(r#"42/notifications,["something-else",{}]"#)).await.unwrap();
            ws.send(Message::text(r#"42/notifications,["notification","{\"value\":[{\"subscriptionId\":\"b\"}]}"]"#)).await.unwrap();
            ws.close(None).await.unwrap();
        })
        .await;
        let mut s = PushSocket::connect(&url).await.unwrap();
        assert_eq!(s.next().await.unwrap().subscription_id.as_deref(), Some("a"));
        assert_eq!(s.next().await.unwrap().subscription_id.as_deref(), Some("b"));
        assert!(matches!(s.next().await, Err(PushError::Closed(_))));
    }

    #[tokio::test]
    async fn silent_server_times_out() {
        let url = fake_server(|ws| async move {
            tokio::time::sleep(Duration::from_secs(5)).await;
            drop(ws);
        })
        .await;
        let mut s = PushSocket::connect(&url).await.unwrap();
        // pingInterval + pingTimeout = 300 ms in the fake.
        assert!(matches!(s.next().await, Err(PushError::HeartbeatTimeout(d)) if d == Duration::from_millis(300)));
    }

    #[tokio::test]
    async fn namespace_disconnect_is_closed() {
        let url = fake_server(|mut ws| async move {
            ws.send(Message::text("41/notifications,")).await.unwrap();
            tokio::time::sleep(Duration::from_secs(1)).await;
        })
        .await;
        let mut s = PushSocket::connect(&url).await.unwrap();
        assert!(matches!(s.next().await, Err(PushError::Closed(_))));
    }
}
