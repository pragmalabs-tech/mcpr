//! # mcpr-integrations
//!
//! External integrations and local persistence for the mcpr proxy.
//!
//! This crate connects mcpr to external services (cloud dashboard) and
//! provides local request storage (SQLite) for CLI observability commands.
//!
//! ## Modules
//!
//! - [`cloud_client`]: HTTP client for the mcpr Cloud API. Handles
//!   authentication, project/server/endpoint CRUD, and token management.
//!   Used by the CLI setup flow (`mcpr proxy setup`).
//!
//! - [`sinks`]: `EventSink` implementations registered on the event bus.
//!   - `StderrSink` — prints `ProxyEvent::Request` events to stderr
//!     (json or pretty `LogFormat`).
//!   - `SqliteSink` — adapts `ProxyEvent` → `StoreEvent` and forwards to
//!     the SQLite `Store` for local persistence.
//!   - `CloudSink` — batches proxy events and POSTs them to cloud.mcpr.app
//!     with retry and exponential backoff.
//!   - `CloudSinkConfig` — endpoint URL, token, server slug, batch/flush tuning.
//!
//! - [`store`]: SQLite persistence and query layer. Pure storage logic —
//!   the `EventSink` adapter lives in [`sinks::sqlite_sink`].
//!   - `Store` — background writer with async mpsc channel
//!   - `QueryEngine` — read-only queries (logs, slow, stats, sessions, clients,
//!     schema, session detail)
//!   - `FileStoreConfig` — `[store]` TOML config with `ModuleConfig` validation
//!
//! ## Module Structure
//!
//! ```text
//! mcpr-integrations/src/
//! +-- lib.rs              # Crate root, re-exports
//! +-- cloud_client.rs     # Cloud API client (auth, CRUD, tokens)
//! +-- sinks/
//! |   +-- mod.rs          # Module root, re-exports
//! |   +-- stderr_sink.rs  # StderrSink (LogFormat: json|pretty)
//! |   +-- sqlite_sink.rs  # SqliteSink (ProxyEvent → StoreEvent adapter)
//! |   +-- cloud_sink.rs   # CloudSink (batched HTTP POST with retry)
//! +-- store/              # Pure SQLite logic (no EventSink impl here)
//!     +-- mod.rs          # Module root, re-exports
//!     +-- config.rs       # FileStoreConfig, ModuleConfig impl
//!     +-- db.rs           # SQLite connection, WAL setup, migrations
//!     +-- duration.rs     # Human-friendly duration parsing (1h, 7d)
//!     +-- engine.rs       # Store struct (open, record, shutdown)
//!     +-- event.rs        # StoreEvent, RequestEvent, SessionEvent
//!     +-- path.rs         # DB path resolution (config > env > platform)
//!     +-- schema.rs       # SQL DDL, indexes, prepared statements
//!     +-- writer.rs       # Background writer thread, batch flush
//!     +-- query/          # Read-only query engine
//!         +-- mod.rs      # QueryEngine struct
//!         +-- logs.rs     # Request log queries
//!         +-- slow.rs     # Slow call queries
//!         +-- stats.rs    # Per-tool aggregation with p95
//!         +-- sessions.rs # Session list with active filter
//!         +-- session_detail.rs # Single-session drill-down
//!         +-- clients.rs  # Client aggregation
//!         +-- schema.rs   # MCP schema queries
//!         +-- store_ops.rs# Store stats and vacuum
//! ```

pub mod cloud_client;
pub mod sinks;
pub mod store;

pub use sinks::{
    CloudSink, CloudSinkConfig, LogFormat, SqliteSink, StderrSink, SyncCallback, SyncStatus,
};
