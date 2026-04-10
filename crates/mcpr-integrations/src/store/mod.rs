//! SQLite-based request storage engine and query layer for mcpr.
//!
//! Every MCP request flowing through the proxy is recorded here, and all
//! CLI observability commands (`mcpr proxy logs`, `mcpr proxy slow`, etc.)
//! read from here.
//!
//! This is a local event sink — the same pattern as [`crate::emitter`], but
//! writing to SQLite instead of an external service.
//!
//! # Architecture
//!
//! ```text
//! Proxy hot path ──► Store::record() ──► mpsc channel ──► Writer thread ──► SQLite (WAL)
//!                    (non-blocking)       (10k buffer)     (batch flush)
//!
//! CLI commands   ──► QueryEngine ──► read-only SQLite connection
//! ```

pub mod config;
pub mod db;
pub mod duration;
pub mod engine;
pub mod event;
pub mod path;
pub mod query;
pub mod schema;
pub mod writer;

// Re-export the main public types for convenience.
pub use config::FileStoreConfig;
pub use duration::{parse_duration, since_to_cutoff_ms};
pub use engine::{Store, StoreConfig, StoreError};
pub use event::{RequestEvent, RequestStatus, SessionEvent, StoreEvent};
pub use query::QueryEngine;
