//! `looper-sharepoint-engine`: everything needed to keep SharePoint libraries synced, as a library.
//!
//! Two ways to use it:
//!
//! - **One-shot**: [`sync_library`] runs a single sync of one library and returns a [`Report`].
//! - **Long-running**: [`Service::start`] schedules runs for every configured library (polling with jitter,
//!   failure backoff, throttle awareness), keeps run history, and exposes [`Status`] snapshots and an [`Event`]
//!   stream for a UI to render.
//!
//! The library does no windowing, daemonizing, signal handling or terminal output; the host application owns all of
//! that. `looper-sharepoint run` (CLI + TUI) and `looper-sharepoint-tray` are two such hosts.
//!
//! ```no_run
//! # async fn demo() -> anyhow::Result<()> {
//! use looper_sharepoint_engine::{Service, ServiceConfig, account, core::config::{Config, data_dir}};
//! let config = Config::load(&Config::default_path())?;
//! let tokens = account::token_provider(&config, &data_dir());
//! let svc = Service::start(ServiceConfig { config, tokens, data_dir: data_dir() })?;
//! let mut events = svc.subscribe();
//! while let Ok(ev) = events.recv().await {
//!     println!("{ev:?} -> {}", svc.status().headline());
//! }
//! # Ok(()) }
//! ```

pub mod access;
pub mod account;
#[cfg(unix)]
pub mod cli_link;
pub mod index;
mod lock;
mod service;
mod sync;
pub mod types;

pub use lock::{HostLock, LockError};
pub use service::{MAX_RETRY_DELAY, SIGN_IN_RECHECK, Service, ServiceConfig, needs_sign_in};
pub use sync::{Issue, MassDeleteRefused, ProgressFn, Report, Resolved, RunOptions, TMP_DIR, no_progress, resolve, sync_library};
pub use types::*;

/// The crates the engine is built on, re-exported so a host needs only this one dependency.
pub use looper_sharepoint_auth as auth;
pub use looper_sharepoint_core as core;
pub use looper_sharepoint_graph as graph;
