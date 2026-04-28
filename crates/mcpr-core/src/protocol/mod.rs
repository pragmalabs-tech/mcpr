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
//! +-- mcp.rs             JSON-RPC envelope + MCP 2025-11-25 method taxonomy
//! +-- schema.rs          Discovery type definitions + canonical hash view
//! +-- session.rs         Session lifecycle, SessionStore trait, MemorySessionStore
//! ```
//!
//! ## Dependencies
//!
//! Minimal: `serde`, `serde_json`, `chrono`, `dashmap`. No HTTP framework deps.

pub mod mcp;
pub mod schema;
pub mod session;
