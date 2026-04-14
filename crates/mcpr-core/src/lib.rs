//! Core types, traits, and shared foundations for mcpr crates.
//!
//! `mcpr-core` is the foundation of the mcpr workspace. Every other mcpr crate
//! depends on it for shared abstractions, protocol types, and proxy logic.
//!
//! ## Modules
//!
//! - [`config`]: Module configuration trait and validation types.
//! - [`event`]: Proxy event types (`ProxyEvent` enum) and sink trait (`EventSink`).
//! - [`protocol`]: JSON-RPC 2.0 parsing, MCP method classification, session
//!   management, and schema capture/diffing.
//! - [`proxy`]: Request routing, upstream forwarding, SSE streaming, CSP
//!   rewriting, and proxy runtime state.
//! - [`time`]: Shared time formatting utilities.

pub mod config;
pub mod event;
pub mod protocol;
pub mod proxy;
pub mod time;
