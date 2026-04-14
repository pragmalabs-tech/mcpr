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
//!   Used by the CLI setup wizard (`mcpr proxy setup`).
//!
//! - [`emitter`]: Cloud event sink that implements `EventSink` from mcpr-core.
//!   - `CloudSink` — batches proxy events and POSTs them to cloud.mcpr.app
//!     with retry and exponential backoff.
//!   - `CloudSinkConfig` — endpoint URL, token, server slug, batch/flush tuning.
//!
//! - [`store`]: SQLite-based request storage engine and query layer powering
//!   all CLI observability commands.
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
//! +-- emitter/
//! |   +-- mod.rs          # Module root, re-exports
//! |   +-- cloud_sink.rs   # CloudSink (batched HTTP POST with retry)
//! +-- store/
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
pub mod emitter;
pub mod store;

pub use emitter::{CloudSink, CloudSinkConfig, SyncCallback, SyncStatus};
