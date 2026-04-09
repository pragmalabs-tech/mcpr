//! Query engine — read-only access to the storage database.
//!
//! All CLI observability commands (`mcpr proxy logs`, `mcpr proxy slow`, etc.)
//! are thin wrappers around [`QueryEngine`] methods. Each method executes a
//! parameterized SQL query and maps rows to typed result structs.
//!
//! The query engine opens its own read-only connection to the database.
//! WAL mode ensures this never blocks the background writer.

pub mod clients;
pub mod logs;
pub mod sessions;
pub mod slow;
pub mod stats;
pub mod store_ops;

use rusqlite::Connection;
use std::path::Path;

use super::db;

/// Read-only query interface to the storage database.
///
/// Opens a separate connection from the writer — WAL mode allows
/// concurrent readers without blocking writes.
pub struct QueryEngine {
    conn: Connection,
}

impl QueryEngine {
    /// Open a query connection to the database at the given path.
    ///
    /// The connection uses the same WAL pragmas as the writer for
    /// consistent read performance.
    pub fn open(db_path: &Path) -> Result<Self, rusqlite::Error> {
        let conn = db::open_connection(db_path)?;
        Ok(QueryEngine { conn })
    }

    /// Get a reference to the underlying connection (for query methods).
    pub(crate) fn conn(&self) -> &Connection {
        &self.conn
    }
}
