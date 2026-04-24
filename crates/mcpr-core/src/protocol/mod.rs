//! # mcpr-protocol
//!
//! MCP specification layer: JSON-RPC 2.0, MCP message taxonomy, schema
//! capture primitives, and session lifecycle. Zero coupling to HTTP
//! frameworks or proxy logic — this module describes what MCP *is*, not
//! how mcpr proxies it.
//!
//! ## Module layout
//!
//! ```text
//! protocol/
//! +-- jsonrpc.rs         JSON-RPC 2.0 envelope, id, error, lazy typed views
//! +-- mcp.rs             MCP 2025-11-25 message taxonomy + classify_client/server
//! +-- schema.rs          Pagination merge/diff + is_schema_method
//! +-- schema_manager/    Per-upstream versioned schema snapshots
//! +-- session.rs         Session lifecycle, SessionStore trait, MemorySessionStore
//! ```
//!
//! ## Dependencies
//!
//! Minimal: `serde`, `serde_json`, `chrono`, `dashmap`. No HTTP framework deps.

pub mod jsonrpc;
pub mod mcp;
pub mod schema;
pub mod schema_manager;
pub mod session;
