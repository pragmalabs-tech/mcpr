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
//! - [`proxy`]: Full MCP proxy engine — per-request pipeline (parse →
//!   route → middleware → forward → emit), [`proxy::ProxyState`] runtime,
//!   widget bundle serving, CSP rewriting, SSE, forwarding, per-proxy health.
//! - [`time`]: Shared time formatting utilities.

pub mod auth;
pub mod config;
pub mod env;
pub mod event;
pub mod protocol;
pub mod proxy2;
pub mod timer;
pub mod utils;
