//! # mcpr-tunnel
//!
//! Tunnel client and self-hosted relay server for mcpr.
//!
//! This crate provides secure tunneling so that locally-running MCP proxies
//! can be reached from the public internet (e.g., for Claude Desktop or
//! ChatGPT to connect to a local MCP server).
//!
//! ## Responsibilities
//!
//! - **Tunnel client** (`client`): Establishes a WebSocket connection to a
//!   relay server, registers a subdomain, and forwards incoming HTTP requests
//!   to the local proxy port.
//!
//! - **Relay server** (`relay`): A standalone server that accepts tunnel
//!   client connections, assigns subdomains, and proxies incoming HTTP
//!   requests through the WebSocket tunnel. Supports multiple auth modes
//!   (open, static token, external provider).
//!
//! - **Wire protocol** (`protocol`): Shared message types for client-relay
//!   communication: `TunnelRequest`, `TunnelResponse`, `RegisterRequest`,
//!   `RegisterAck`, `SubdomainOffer`, `SubdomainPick`.
//!
//! ## Module Structure
//!
//! ```text
//! mcpr-tunnel/src/
//! +-- lib.rs          # Crate root, re-exports
//! +-- client.rs       # start_tunnel_client(), TunnelStatusCallback trait
//! +-- protocol.rs     # Wire protocol types (serde-serializable)
//! +-- relay/
//!     +-- mod.rs      # start_relay() entry point
//!     +-- config.rs   # RelayConfig
//!     +-- ...         # Auth, domain management, WebSocket handling
//! ```
//!
//! ## Dependencies
//!
//! `axum`, `tokio`, `tokio-tungstenite`, `reqwest`, `serde`, `dashmap`, `uuid`.
//! Completely standalone — no dependency on other mcpr crates.

pub mod client;
pub mod protocol;
pub mod relay;

pub use client::{TunnelStatusCallback, start_tunnel_client};
pub use protocol::*;
pub use relay::config::RelayConfig;
