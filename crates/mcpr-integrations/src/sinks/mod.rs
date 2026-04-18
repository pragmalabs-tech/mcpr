//! `EventSink` implementations — where `ProxyEvent`s get fanned out to
//! their destinations.
//!
//! - [`stderr_sink`]: real-time console output.
//! - [`sqlite_sink`]: persists events into the local SQLite store
//!   (`crate::store`).
//! - [`cloud_sink`]: batches + POSTs events to cloud.mcpr.app.

pub mod cloud_sink;
pub mod sqlite_sink;
pub mod stderr_sink;

pub use cloud_sink::{CloudSink, CloudSinkConfig, SyncCallback, SyncStatus};
pub use sqlite_sink::SqliteSink;
pub use stderr_sink::{LogFormat, StderrSink};
