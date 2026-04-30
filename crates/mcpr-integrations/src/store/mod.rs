//! SQLite-based request storage engine and query layer for mcpr.
//!
//! Pure persistence logic: open the DB, write events, run read-only
//! queries. The `EventSink` adapter that plugs this store into the event
//! bus lives in [`crate::sinks::sqlite_sink`].
//!
//! # Architecture
//!
//! ```text
//! SqliteSink ──► Store::record() ──► mpsc channel ──► Writer thread ──► SQLite (WAL)
//!                (non-blocking)       (10k buffer)     (batch flush)
//!
//! CLI commands ──► QueryEngine ──► read-only SQLite connection
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
pub use event::StoreEvent;
pub use query::QueryEngine;
