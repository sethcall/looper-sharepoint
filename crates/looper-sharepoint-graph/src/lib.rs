//! Thin Microsoft Graph client for looper-sharepoint.

mod api;
mod client;
pub mod models;
pub mod push;

pub use api::{CHUNK, UploadTarget};
pub use client::{GraphClient, GraphError, KindStats, RequestKind, Result, Stats};
