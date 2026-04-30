//! SQLite connection management and one-shot schema bootstrap.
//!
//! - Opens a connection with WAL + performance pragmas (used by both
//!   the writer and the read-only query engine).
//! - Runs `CREATE_ALL_SQL` once on first open; idempotent on subsequent
//!   starts via `IF NOT EXISTS`.
//!
//! mcpr is pre-1.0 — no migration runner. Schema layout changes happen
//! by bumping `SCHEMA_VERSION` and dropping/recreating the dev database.
//!
//! # WAL mode
//!
//! WAL (Write-Ahead Logging) is enabled on every connection so the
//! background writer and read-only CLI queries can run concurrently
//! without blocking each other.

use std::path::Path;

use rusqlite::Connection;

use super::schema;

/// Open a SQLite connection with WAL mode and performance pragmas.
///
/// Used by both the background writer (read-write) and the query engine
/// (read-only). The pragmas are safe for both.
///
/// # Pragmas
///
/// - `journal_mode = WAL`: concurrent reads during writes.
/// - `synchronous = NORMAL`: ~3x faster than FULL with WAL; at most one
///   batch (200ms) of events lost on OS crash.
/// - `cache_size = -64000`: 64MB page cache.
/// - `temp_store = MEMORY`: temp tables/indexes in RAM.
/// - `busy_timeout = 5000`: wait up to 5s for locks before failing.
pub fn open_connection(path: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open(path)?;

    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         PRAGMA cache_size = -64000;
         PRAGMA temp_store = MEMORY;
         PRAGMA busy_timeout = 5000;",
    )?;

    Ok(conn)
}

/// Create the schema (tables, indexes, view) and stamp the running mcpr
/// binary version into the `meta` table.
///
/// Idempotent: every `CREATE TABLE` / `CREATE INDEX` / `CREATE VIEW`
/// uses `IF NOT EXISTS`, and the `meta` seeds use `INSERT OR IGNORE`.
/// Safe to call from both `Store::open` (read-write) and
/// `QueryEngine::open` (read-only callers may still need the schema if
/// they hit a fresh database before the writer ran).
pub fn init_schema(conn: &Connection, mcpr_version: &str) -> rusqlite::Result<()> {
    conn.execute_batch(schema::CREATE_ALL_SQL)?;
    conn.execute_batch(schema::META_SEED_SQL)?;
    conn.execute(
        schema::UPSERT_MCPR_VERSION_SQL,
        rusqlite::params![mcpr_version],
    )?;
    Ok(())
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;

    #[test]
    fn init_schema__fresh_db_creates_all_tables() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");

        let conn = open_connection(&db_path).unwrap();
        init_schema(&conn, "0.3.0-test").unwrap();

        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE type = 'table'
                   AND name IN ('requests','responses','sessions','schema_items','schema_changes','meta')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 6);

        let view_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'view' AND name = 'request_log'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(view_count, 1);

        let version: String = conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'schema_version'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(version, schema::SCHEMA_VERSION);

        let mcpr_ver: String = conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'mcpr_version'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(mcpr_ver, "0.3.0-test");
    }

    #[test]
    fn init_schema__idempotent_on_repeat_call() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");

        let conn = open_connection(&db_path).unwrap();
        init_schema(&conn, "0.3.0").unwrap();
        init_schema(&conn, "0.3.1").unwrap();

        let mcpr_ver: String = conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'mcpr_version'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(mcpr_ver, "0.3.1");
    }
}
