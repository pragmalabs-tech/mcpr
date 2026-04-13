//! Core types, traits, and shared foundations for mcpr crates.
//!
//! `mcpr-core` is the bottom of the dependency graph — every other mcpr crate
//! can depend on it for shared abstractions without pulling in heavy dependencies.
//!
//! Provides:
//! - [`config`]: Module configuration trait and validation types.
//! - [`event`]: Proxy event types (`ProxyEvent` enum) and sink trait (`EventSink`).

pub mod config;
pub mod event;
pub mod time;
