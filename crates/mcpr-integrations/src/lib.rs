//! # mcpr-integrations
//!
//! External integrations for the mcpr proxy: event emitters, and (future)
//! metrics sinks, feature flags, and analytics providers.
//!
//! This crate is the extension point for connecting mcpr to external services.
//! Each integration category lives in its own module with a shared trait and
//! one or more implementations. All integrations are runtime-configured via
//! `mcpr.toml` — no compile-time feature flags needed.
//!
//! ## Current Integrations
//!
//! - **Emitters** (`emitter` module): Structured event emission for MCP
//!   tool calls, sessions, heartbeats, and errors.
//!   - `StdoutEmitter` — JSON lines to stdout (default)
//!   - `CloudEmitter` — batched HTTPS POST to cloud.mcpr.app
//!   - `NoopEmitter` — disabled events
//!
//! ## Future Integrations (planned)
//!
//! - **Metrics** (`metrics` module): `MetricsSink` trait for Prometheus,
//!   Grafana, Datadog, etc.
//! - **Feature flags** (`flags` module): `FeatureFlags` trait for Statsig,
//!   LaunchDarkly, etc.
//! - **Analytics** (`analytics` module): Usage analytics and reporting.
//!
//! ## Module Structure
//!
//! ```text
//! mcpr-integrations/src/
//! +-- lib.rs              # Crate root, re-exports
//! +-- emitter/
//!     +-- mod.rs          # Module root, re-exports
//!     +-- traits.rs       # EventEmitter trait, NoopEmitter
//!     +-- event.rs        # McprEvent struct, EventType, EventStatus
//!     +-- stdout.rs       # StdoutEmitter implementation
//!     +-- cloud.rs        # CloudEmitter (batched HTTP POST)
//! ```
//!
//! ## Adding a New Integration
//!
//! 1. Create a new module (e.g., `src/metrics/`)
//! 2. Define a trait (e.g., `MetricsSink`)
//! 3. Implement it for each backend
//! 4. Add a TOML config section in `mcpr-cli`
//! 5. Wire it up in CLI startup

pub mod emitter;

// Re-export emitter types at crate root for ergonomic access.
pub use emitter::*;
