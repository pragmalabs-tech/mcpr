//! `EventSink` implementations — where `ProxyEvent`s get fanned out to
//! their destinations.
//!
//! - [`stderr_sink`]: real-time console output.
//! - [`sqlite_sink`]: persists events into the local SQLite store
//!   (`crate::store`).
//! - [`cloud_sink`]: batches + POSTs events to cloud.mcpr.app.

pub mod stderr_sink;

pub use stderr_sink::StderrSink;
