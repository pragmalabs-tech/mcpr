//! SQLite-based request storage engine and query layer for mcpr.
//!
//! Every MCP request flowing through the proxy is recorded here, and all
//! CLI observability commands (`mcpr proxy logs`, `mcpr proxy slow`, etc.)
//! read from here.
//!
//! This is a local event sink — the same pattern as [`crate::emitter`], but
//! writing to SQLite instead of cloud or stdout.
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
pub mod event;
pub mod path;
pub mod schema;
pub mod store;
pub mod writer;

// Re-export the main public types for convenience.
pub use config::FileStoreConfig;
pub use event::{RequestEvent, RequestStatus, SessionEvent, StoreEvent};
pub use store::{Store, StoreConfig, StoreError};
