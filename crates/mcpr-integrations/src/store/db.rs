//! SQLite connection management, WAL setup, and schema migrations.
//!
//! This module handles the low-level database lifecycle:
//! - Opening a connection with the right pragmas for performance and safety.
//! - Running schema migrations on first open or version upgrade.
//! - Providing separate read-only connections for the query layer.
//!
//! # WAL mode
//!
//! WAL (Write-Ahead Logging) is enabled on every connection. This gives:
//! - Concurrent readers while the background writer is writing.
//! - Better write throughput for the batch write pattern.
//! - Crash safety — no corruption on unclean shutdown.
//!
//! # Thread safety
//!
//! `rusqlite::Connection` is `!Send`. The writer owns its connection on a
//! dedicated OS thread. Query commands open their own read-only connections.

use rusqlite::Connection;
use std::path::Path;

use super::schema;

/// Open a SQLite connection with WAL mode and performance pragmas.
///
/// This is used by both the background writer (read-write) and the query
/// engine (read-only). The pragmas are safe for both use cases.
///
/// # Pragmas
///
/// - `journal_mode = WAL`: enables concurrent reads during writes.
/// - `synchronous = NORMAL`: safe with WAL, ~3x faster than FULL.
///   Acceptable durability trade-off for a local request log — at most
///   one batch (200ms) of events could be lost on OS crash.
/// - `cache_size = -64000`: 64MB page cache in memory. Improves read
///   performance for repeated queries (e.g., `--follow` polling).
/// - `temp_store = MEMORY`: temp tables and indexes in memory, not disk.
/// - `busy_timeout = 5000`: wait up to 5s for locks instead of failing
///   immediately. Prevents SQLITE_BUSY errors when CLI queries overlap
///   with writer flushes.
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

/// Run schema migrations to bring the database up to the current version.
///
/// Checks the `schema_version` in the `meta` table. If the table doesn't
/// exist (fresh database), runs the full V1 schema. Future versions will
/// add incremental migrations (V1 → V2, V2 → V3, etc.).
///
/// This is called once on `Store::open()` before spawning the writer.
pub fn run_migrations(conn: &Connection, mcpr_version: &str) -> rusqlite::Result<()> {
    let version = get_schema_version(conn);

    if version < 1 {
        conn.execute_batch(schema::V1_SCHEMA)?;
        conn.execute_batch(schema::V1_META_SEED)?;
    }

    // Always update the mcpr binary version on startup.
    conn.execute(schema::UPSERT_MCPR_VERSION, rusqlite::params![mcpr_version])?;

    Ok(())
}

/// Read the current schema version from the `meta` table.
///
/// Returns 0 if the `meta` table doesn't exist (fresh database)
/// or if the `schema_version` key is missing.
fn get_schema_version(conn: &Connection) -> u32 {
    // Check if the meta table exists at all.
    let table_exists: bool = conn
        .query_row(
            "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type = 'table' AND name = 'meta'",
            [],
            |row| row.get(0),
        )
        .unwrap_or(false);

    if !table_exists {
        return 0;
    }

    conn.query_row(
        "SELECT value FROM meta WHERE key = 'schema_version'",
        [],
        |row| {
            let v: String = row.get(0)?;
            Ok(v.parse::<u32>().unwrap_or(0))
        },
    )
    .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_and_migrate_fresh_db() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");

        let conn = open_connection(&db_path).unwrap();
        run_migrations(&conn, "0.3.0-test").unwrap();

        // Verify tables exist
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name IN ('requests', 'sessions', 'meta')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 3, "all three tables should exist");

        // Verify schema version
        let version: String = conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'schema_version'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(version, "1");

        // Verify mcpr version
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
    fn migrations_are_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");

        let conn = open_connection(&db_path).unwrap();
        run_migrations(&conn, "0.3.0").unwrap();
        // Running again should not fail.
        run_migrations(&conn, "0.3.1").unwrap();

        let mcpr_ver: String = conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'mcpr_version'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(mcpr_ver, "0.3.1", "version should be updated on re-run");
    }
}
