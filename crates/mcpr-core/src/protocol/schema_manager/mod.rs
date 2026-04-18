//! Schema manager: the top-level per-upstream view of an MCP server.
//!
//! `SchemaManager` owns the canonical picture of what an upstream MCP
//! server exposes — its capabilities, tools, resources, prompts — and
//! how that picture changes over time. Other subsystems (sessions,
//! tool hiding, custom tools, audit, cloud sync) read from it.
//!
//! This module consists of:
//!
//! - [`SchemaVersion`] / [`SchemaVersionId`]: immutable, content-hashed
//!   snapshot of one method's merged payload.
//! - [`SchemaStore`] / [`MemorySchemaStore`]: pluggable persistence
//!   with a bounded in-memory default.
//! - [`SchemaManager`]: ingest + query + stale tracking. Calls existing
//!   `crate::protocol::schema` helpers for pagination and diffing.
//! - [`SchemaScanner`]: trait for active discovery (`Standalone` or
//!   `Attached` mode). No concrete implementation in this step.
//! - [`ScanTrigger`]: enumerates the reasons a scan is initiated.

mod manager;
mod scanner;
mod store;
mod version;

pub use manager::SchemaManager;
pub use scanner::{ScanError, ScanMode, ScanResult, SchemaScanner};
pub use store::{MemorySchemaStore, SchemaStore};
pub use version::{SchemaVersion, SchemaVersionId};

use serde::Serialize;

/// Why a scan was started.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ScanTrigger {
    /// Proxy startup — initial discovery run.
    ProxyStart,
    /// TTL on the current version elapsed.
    TtlElapsed,
    /// `initialize` response saw a different `serverInfo`.
    ServerIdentityChanged,
    /// `notifications/tools/list_changed` received.
    ListChangedNotification,
    /// Operator-triggered manual rescan.
    Manual,
}
