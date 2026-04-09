//! Core types, traits, and shared foundations for mcpr crates.
//!
//! `mcpr-core` is the bottom of the dependency graph — every other mcpr crate
//! can depend on it for shared abstractions without pulling in heavy dependencies.
//!
//! Currently provides:
//! - [`config`]: Module configuration trait and validation types.

pub mod config;
