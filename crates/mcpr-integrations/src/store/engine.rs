//! The `Store` — public API for the storage engine.
//!
//! `Store` is the single entry point for the rest of mcpr to interact with
//! persistent storage. It handles:
//!
//! - Opening the database and running migrations.
//! - Spawning the background writer thread.
//! - Providing a non-blocking `record()` method for the proxy hot path.
//! - Graceful shutdown with guaranteed flush of pending events.
//!
//! # Usage
//!
//! ```rust,ignore
//! let store = Store::open(StoreConfig {
//!     db_path: PathBuf::from("/tmp/mcpr.db"),
//!     mcpr_version: "0.3.0".into(),
//! })?;
//!
//! // Hot path — non-blocking, fire-and-forget.
//! store.record(StoreEvent {
//!     ts: chrono::Utc::now().timestamp_millis(),
//!     proxy: proxy_arc.clone(),
//!     event: proxy_event,
//! });
//!
//! // Shutdown — blocks until writer drains pending events.
//! store.shutdown();
//! ```

use std::path::PathBuf;
use std::thread::JoinHandle;

use tokio::sync::mpsc;

use super::db;
use super::event::StoreEvent;
use super::path;
use super::writer;

/// Channel capacity — how many events can be buffered before the hot path
/// starts dropping them.
///
/// At 1,000 requests/second this is a 10-second buffer. More than enough
/// to absorb any write latency spike from SQLite.
const CHANNEL_CAPACITY: usize = 10_000;

/// Configuration for opening the store.
pub struct StoreConfig {
    /// Path to the SQLite database file.
    /// The parent directory is created automatically if it doesn't exist.
    pub db_path: PathBuf,

    /// The current mcpr binary version (e.g., "0.3.0").
    /// Written to the `meta` table on every startup for diagnostics.
    pub mcpr_version: String,
}

/// Handle to the storage engine.
///
/// Cheap to clone (sender + Arc internally). The proxy holds one, and
/// CLI query commands can open their own read-only connections separately.
pub struct Store {
    /// Channel sender for the background writer. Non-blocking `try_send`.
    tx: mpsc::Sender<StoreEvent>,

    /// Join handle for the writer thread. Used for graceful shutdown.
    writer_handle: Option<JoinHandle<()>>,

    /// Database path — needed by the query engine to open read-only connections.
    db_path: PathBuf,
}

impl Store {
    /// Open or create the database, run migrations, and spawn the writer thread.
    ///
    /// This is called once on proxy startup. It:
    /// 1. Creates the parent directory if needed.
    /// 2. Opens a read-write connection and runs schema migrations.
    /// 3. Spawns the background writer on a dedicated OS thread.
    /// 4. Returns a `Store` handle for recording events.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The parent directory can't be created (permissions).
    /// - SQLite can't open the file (disk full, corrupt file).
    /// - Schema migrations fail (shouldn't happen on fresh DBs).
    pub fn open(config: StoreConfig) -> Result<Self, StoreError> {
        // Ensure parent directory exists.
        path::ensure_parent_dir(&config.db_path)
            .map_err(|e| StoreError::Io(format!("failed to create db directory: {e}")))?;

        // Open connection and run migrations on the current thread.
        // This validates that the DB is usable before we hand off to the writer.
        let conn = db::open_connection(&config.db_path)
            .map_err(|e| StoreError::Sqlite(format!("failed to open database: {e}")))?;

        db::init_schema(&conn, &config.mcpr_version)
            .map_err(|e| StoreError::Sqlite(format!("schema init failed: {e}")))?;

        // Create the event channel.
        let (tx, rx) = mpsc::channel::<StoreEvent>(CHANNEL_CAPACITY);

        // Spawn the writer on a dedicated OS thread.
        // rusqlite::Connection is !Send, so it must stay on one thread.
        // The connection is moved into the thread — nobody else writes.
        let writer_handle = std::thread::Builder::new()
            .name("mcpr-store-writer".into())
            .spawn(move || {
                writer::run_writer_loop(conn, rx);
            })
            .map_err(|e| StoreError::Io(format!("failed to spawn writer thread: {e}")))?;

        Ok(Store {
            tx,
            writer_handle: Some(writer_handle),
            db_path: config.db_path,
        })
    }

    /// Record an event — non-blocking, fire-and-forget.
    ///
    /// If the channel is full (back-pressure), the event is silently dropped.
    /// This is intentional: a busy proxy must never block on storage writes.
    /// Dropped events are a signal that the writer can't keep up — in practice
    /// this should never happen at normal MCP request rates.
    pub fn record(&self, event: StoreEvent) {
        // try_send returns Err if the channel is full or closed.
        // We intentionally ignore both — the proxy must not block.
        let _ = self.tx.try_send(event);
    }

    /// Get the database path for opening read-only query connections.
    pub fn db_path(&self) -> &PathBuf {
        &self.db_path
    }

    /// Graceful shutdown — close the channel and wait for the writer to flush.
    ///
    /// Call this on proxy shutdown (after stopping new requests, before exiting).
    /// Blocks the current thread until all pending events are written to SQLite.
    ///
    /// After this returns, the database file is consistent and safe to read.
    pub fn shutdown(&mut self) {
        // Drop the sender to signal the writer that no more events are coming.
        // The writer will drain any remaining events and exit.
        //
        // We replace tx with a closed channel — any subsequent record() calls
        // will silently fail, which is correct during shutdown.
        let (dead_tx, _) = mpsc::channel(1);
        let old_tx = std::mem::replace(&mut self.tx, dead_tx);
        drop(old_tx);

        // Wait for the writer thread to finish.
        if let Some(handle) = self.writer_handle.take()
            && let Err(e) = handle.join()
        {
            eprintln!("mcpr-store: writer thread panicked: {e:?}");
        }
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        // Best-effort shutdown if not already called.
        // In normal usage, shutdown() is called explicitly before drop.
        if self.writer_handle.is_some() {
            self.shutdown();
        }
    }
}

/// Errors from store operations.
#[derive(Debug)]
pub enum StoreError {
    /// Filesystem error (directory creation, permissions).
    Io(String),
    /// SQLite error (open, migration, query).
    Sqlite(String),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Io(msg) => write!(f, "store I/O error: {msg}"),
            StoreError::Sqlite(msg) => write!(f, "store SQLite error: {msg}"),
        }
    }
}

impl std::error::Error for StoreError {}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use std::sync::Arc;

    use chrono::Utc;
    use mcpr_core::event::ProxyEvent;
    use mcpr_core::protocol::{
        Request,
        mcp::{ClientMethod, JsonRpcRequest, JsonRpcVersion, RequestId, ToolsMethod},
        session::{SessionInfo, SessionState},
    };
    use serde_json::json;

    use super::*;

    #[test]
    fn store__open_record_shutdown() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");

        let mut store = Store::open(StoreConfig {
            db_path: db_path.clone(),
            mcpr_version: "test".into(),
        })
        .unwrap();

        let proxy: Arc<str> = Arc::from("test-proxy");
        let now = Utc::now();

        let info = SessionInfo {
            id: "s1".into(),
            state: SessionState::Active,
            client_info: Some(mcpr_core::protocol::mcp::ClientInfo {
                name: "test-client".into(),
                version: Some("0.1".into()),
            }),
            created_at: now,
            last_active: now,
            request_count: 1,
            request_ids: vec![],
        };
        store.record(StoreEvent {
            ts: now.timestamp_millis(),
            proxy: proxy.clone(),
            event: ProxyEvent::Session(Arc::new(info)),
        });

        let parts = http::Request::builder()
            .method("POST")
            .uri("/")
            .header("mcp-session-id", "s1")
            .body(())
            .unwrap()
            .into_parts()
            .0;
        let rpc = JsonRpcRequest {
            jsonrpc: JsonRpcVersion,
            id: RequestId::Number(1),
            method: ClientMethod::Tools(ToolsMethod::Call),
            params: Some(serde_json::Map::from_iter([(
                "name".into(),
                json!("test_tool"),
            )])),
        };
        store.record(StoreEvent {
            ts: now.timestamp_millis() + 1,
            proxy: proxy.clone(),
            event: ProxyEvent::Request(Arc::new(Request::Mcp(parts, rpc))),
        });

        store.shutdown();

        let conn = db::open_connection(&db_path).unwrap();

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM requests", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);

        let tool: String = conn
            .query_row(
                "SELECT tool FROM requests WHERE request_id = '1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(tool, "test_tool");
    }
}
