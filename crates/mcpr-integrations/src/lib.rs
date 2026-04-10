//! # mcpr-integrations
//!
//! External integrations and local persistence for the mcpr proxy.
//!
//! This crate connects mcpr to external services (cloud dashboard) and
//! provides local request storage (SQLite) for CLI observability commands.
//!
//! ## Current Integrations
//!
//! - **Emitters** (`emitter` module): Structured event emission for MCP
//!   tool calls, sessions, heartbeats, and errors.
//!   - `CloudEmitter` — batched HTTPS POST to cloud.mcpr.app
//!   - `NoopEmitter` — disabled events
//!
//! - **Store** (`store` module): SQLite-based request storage engine and
//!   query layer powering all CLI observability commands.
//!   - `Store` — background writer with async mpsc channel
//!   - `QueryEngine` — read-only queries (logs, slow, stats, sessions, clients)
//!   - `FileStoreConfig` — `[store]` TOML config with `ModuleConfig` validation
//!
//! ## Module Structure
//!
//! ```text
//! mcpr-integrations/src/
//! +-- lib.rs              # Crate root, re-exports
//! +-- emitter/
//! |   +-- mod.rs          # Module root, re-exports
//! |   +-- traits.rs       # EventEmitter trait, NoopEmitter
//! |   +-- event.rs        # McprEvent struct, EventType, EventStatus
//! |   +-- cloud.rs        # CloudEmitter (batched HTTP POST)
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
//!         +-- clients.rs  # Client aggregation
//!         +-- store_ops.rs# Store stats and vacuum
//! ```

pub mod emitter;
pub mod store;

// Re-export emitter types at crate root for ergonomic access.
pub use emitter::*;
