//! Optional local control socket for a looper-sharepoint host. See `specs/m2-design.md` §5.
//!
//! A host that embeds [`Service`] calls [`serve`] to let other processes (the `looper-sharepoint` CLI, the latency
//! bench) observe and drive it. Embedding apps that don't want a socket simply don't call it.
//!
//! - Transport: a Unix domain socket in the data dir (mode 0600) on macOS/Linux, a named pipe on Windows. The name is
//!   derived from the data dir, so isolated setups never collide.
//! - Protocol: newline-delimited JSON. One [`Request`] per connection, answered by one [`Response`], except
//!   [`Request::Subscribe`], which streams [`Response::Event`]s until either side hangs up.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use interprocess::local_socket::tokio::{Listener, Stream, prelude::*};
use interprocess::local_socket::{ListenerOptions, Name};
use serde::{Deserialize, Serialize};
use looper_sharepoint_engine::{Event, RunOptions, RunRecord, RunResult, Service, Status, Trigger};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::broadcast::error::RecvError;
use tokio::task::JoinHandle;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    /// Current status plus recent history.
    Status {
        #[serde(default = "default_history")]
        history: usize,
    },
    /// Start a run and return immediately.
    Trigger { library: Option<String> },
    /// Run now and answer when finished.
    Sync {
        library: Option<String>,
        #[serde(default)]
        options: RunOptions,
    },
    /// Stream events.
    Subscribe,
    Pause,
    Resume,
    /// The user signed in (e.g. via `looper-sharepoint login` in another process).
    SignedIn,
    /// Reconnect live updates now, skipping any backoff wait.
    Reconnect { library: Option<String> },
}

fn default_history() -> usize {
    20
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    Ok,
    Status { status: Status, history: Vec<RunRecord> },
    Sync { results: Vec<RunResult> },
    Event { event: Event },
    /// The subscriber fell behind and missed this many events; re-read the status.
    Lagged { missed: u64 },
    Error { message: String },
}

/// The socket name for a data dir.
fn name_for(data_dir: &Path) -> Result<Name<'static>> {
    #[cfg(windows)]
    {
        use interprocess::local_socket::{GenericNamespaced, ToNsName};
        let n = format!("looper-sharepoint-{:016x}", fnv1a(data_dir.to_string_lossy().as_bytes()));
        Ok(n.to_ns_name::<GenericNamespaced>()?)
    }
    #[cfg(not(windows))]
    {
        use interprocess::local_socket::{GenericFilePath, ToFsName};
        Ok(socket_path(data_dir).to_fs_name::<GenericFilePath>()?.into_owned())
    }
}

/// Unix socket paths are limited to ~104 bytes; fall back to the per-user temp dir for deep data dirs.
#[cfg(not(windows))]
fn socket_path(data_dir: &Path) -> PathBuf {
    let p = data_dir.join("host.sock");
    if p.as_os_str().len() < 100 {
        return p;
    }
    std::env::temp_dir().join(format!("looper-sharepoint-{:016x}.sock", fnv1a(data_dir.to_string_lossy().as_bytes())))
}

/// Stable across builds (unlike `DefaultHasher`), so a CLI and a tray built separately agree on the name.
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325u64, |h, b| (h ^ *b as u64).wrapping_mul(0x100000001b3))
}

/// A running control socket. Dropping it stops accepting connections.
pub struct Server {
    task: JoinHandle<()>,
    #[cfg(not(windows))]
    path: PathBuf,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
        #[cfg(not(windows))]
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Serve the control socket for `svc`. Call after [`Service::start`] (which holds the host lock, so any existing
/// socket file is stale and safe to replace).
pub fn serve(svc: Service) -> Result<Server> {
    let data_dir = svc.data_dir().to_owned();
    #[cfg(not(windows))]
    let path = {
        let p = socket_path(&data_dir);
        let _ = std::fs::remove_file(&p);
        p
    };
    let listener = ListenerOptions::new().name(name_for(&data_dir)?).create_tokio().context("creating control socket")?;
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).context("restricting control socket")?;
    }
    let task = tokio::spawn(accept_loop(listener, svc));
    Ok(Server {
        task,
        #[cfg(not(windows))]
        path,
    })
}

async fn accept_loop(listener: Listener, svc: Service) {
    loop {
        match listener.accept().await {
            Ok(conn) => {
                let svc = svc.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle(conn, svc).await {
                        tracing::debug!("control connection: {e:#}");
                    }
                });
            }
            Err(e) => {
                tracing::warn!("control socket accept: {e}");
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
        }
    }
}

async fn handle(conn: Stream, svc: Service) -> Result<()> {
    let (recv, mut send) = conn.split();
    let mut line = String::new();
    BufReader::new(recv).read_line(&mut line).await?;
    let req: Request = match serde_json::from_str(&line) {
        Ok(r) => r,
        Err(e) => return write(&mut send, &Response::Error { message: format!("bad request: {e}") }).await,
    };
    let resp = match req {
        Request::Status { history } => Response::Status { status: svc.status(), history: svc.history(history) },
        Request::Trigger { library } => match svc.trigger(library.as_deref(), Trigger::Manual) {
            Ok(()) => Response::Ok,
            Err(e) => Response::Error { message: format!("{e:#}") },
        },
        Request::Sync { library, options } => match svc.sync_now(library.as_deref(), options).await {
            Ok(results) => Response::Sync { results },
            Err(e) => Response::Error { message: format!("{e:#}") },
        },
        Request::Pause => {
            svc.pause();
            Response::Ok
        }
        Request::Resume => {
            svc.resume();
            Response::Ok
        }
        Request::SignedIn => {
            svc.signed_in();
            Response::Ok
        }
        Request::Reconnect { library } => match svc.reconnect_push(library.as_deref()) {
            Ok(()) => Response::Ok,
            Err(e) => Response::Error { message: format!("{e:#}") },
        },
        Request::Subscribe => {
            let mut rx = svc.subscribe();
            write(&mut send, &Response::Ok).await?;
            loop {
                let resp = match rx.recv().await {
                    Ok(event) => Response::Event { event },
                    Err(RecvError::Lagged(missed)) => Response::Lagged { missed },
                    Err(RecvError::Closed) => return Ok(()),
                };
                write(&mut send, &resp).await?; // fails once the client hangs up
            }
        }
    };
    write(&mut send, &resp).await
}

async fn write(send: &mut (impl AsyncWriteExt + Unpin), resp: &Response) -> Result<()> {
    let mut buf = serde_json::to_vec(resp)?;
    buf.push(b'\n');
    send.write_all(&buf).await?;
    send.flush().await?;
    Ok(())
}

/// A connection to a running host.
pub struct Client {
    reader: BufReader<interprocess::local_socket::tokio::RecvHalf>,
    sender: interprocess::local_socket::tokio::SendHalf,
}

impl Client {
    /// Connect to the host for `data_dir`. `Ok(None)` means no host is running.
    pub async fn connect(data_dir: &Path) -> Result<Option<Self>> {
        match Stream::connect(name_for(data_dir)?).await {
            Ok(s) => {
                let (recv, sender) = s.split();
                Ok(Some(Self { reader: BufReader::new(recv), sender }))
            }
            Err(e) if is_not_running(&e) => Ok(None),
            Err(e) => Err(e).context("connecting to the running looper-sharepoint host"),
        }
    }

    /// Send one request and read one response.
    pub async fn call(mut self, req: &Request) -> Result<Response> {
        self.send(req).await?;
        self.next().await?.context("host closed the connection without answering")
    }

    /// Subscribe; then call [`Client::next`] for each event.
    pub async fn subscribe(mut self) -> Result<Self> {
        self.send(&Request::Subscribe).await?;
        match self.next().await? {
            Some(Response::Ok) => Ok(self),
            other => bail!("unexpected subscribe answer: {other:?}"),
        }
    }

    /// The next response line; `None` when the host hung up.
    pub async fn next(&mut self) -> Result<Option<Response>> {
        let mut line = String::new();
        if self.reader.read_line(&mut line).await? == 0 {
            return Ok(None);
        }
        Ok(Some(serde_json::from_str(&line).with_context(|| format!("bad response from host: {line}"))?))
    }

    async fn send(&mut self, req: &Request) -> Result<()> {
        let mut buf = serde_json::to_vec(req)?;
        buf.push(b'\n');
        self.sender.write_all(&buf).await?;
        self.sender.flush().await?;
        Ok(())
    }
}

/// One request to the host for `data_dir`; `Ok(None)` if no host is running.
pub async fn request(data_dir: &Path, req: &Request) -> Result<Option<Response>> {
    match Client::connect(data_dir).await? {
        Some(c) => Ok(Some(c.call(req).await?)),
        None => Ok(None),
    }
}

fn is_not_running(e: &std::io::Error) -> bool {
    use std::io::ErrorKind::*;
    matches!(e.kind(), NotFound | ConnectionRefused) || e.raw_os_error() == Some(2) // ERROR_FILE_NOT_FOUND (pipe)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_roundtrip_as_json() {
        let r: Request = serde_json::from_str(r#"{"cmd":"sync","library":"qa","options":{"dry_run":true}}"#).unwrap();
        assert!(matches!(r, Request::Sync { library: Some(ref l), ref options } if l == "qa" && options.dry_run));
        let r: Request = serde_json::from_str(r#"{"cmd":"status"}"#).unwrap();
        assert!(matches!(r, Request::Status { history: 20 }));
    }

    #[test]
    fn names_are_stable_and_distinct() {
        assert_eq!(fnv1a(b"abc"), 0xe71fa2190541574b);
        assert_ne!(fnv1a(b"/a"), fnv1a(b"/b"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn socket_roundtrip() {
        use looper_sharepoint_engine::{ServiceConfig, auth::StaticToken, core::config::Config};
        let dir = tempfile::tempdir().unwrap();
        let config = Config::parse("[account]\ntenant = \"t\"\nclient_id = \"c\"\n", Path::new("t.toml")).unwrap();
        let tokens = std::sync::Arc::new(StaticToken("x".into()));
        let svc = Service::start(ServiceConfig { config, tokens, data_dir: dir.path().into() }).unwrap();
        assert!(Client::connect(dir.path()).await.unwrap().is_none(), "no server yet");
        let _server = serve(svc.clone()).unwrap();

        let mut sub = Client::connect(dir.path()).await.unwrap().unwrap().subscribe().await.unwrap();
        assert!(matches!(request(dir.path(), &Request::Pause).await.unwrap(), Some(Response::Ok)));
        match request(dir.path(), &Request::Status { history: 5 }).await.unwrap() {
            Some(Response::Status { status, .. }) => assert!(status.paused),
            other => panic!("{other:?}"),
        }
        let ev = tokio::time::timeout(std::time::Duration::from_secs(5), sub.next()).await.unwrap().unwrap();
        assert!(matches!(ev, Some(Response::Event { event: Event::StatusChanged })), "{ev:?}");
        match request(dir.path(), &Request::Trigger { library: Some("nope".into()) }).await.unwrap() {
            Some(Response::Error { message }) => assert!(message.contains("nope")),
            other => panic!("{other:?}"),
        }
        svc.shutdown().await;
    }

    #[cfg(not(windows))]
    #[test]
    fn long_data_dirs_fall_back_to_temp() {
        let deep = PathBuf::from(format!("/{}", "x".repeat(150)));
        assert!(socket_path(&deep).starts_with(std::env::temp_dir()));
        assert_eq!(socket_path(Path::new("/tmp/d")), PathBuf::from("/tmp/d/host.sock"));
    }
}
