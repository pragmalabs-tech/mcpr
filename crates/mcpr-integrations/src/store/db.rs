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

    if version < 2 {
        conn.execute_batch(schema::V2_SCHEMA)?;
    }

    if version < 3 {
        conn.execute_batch(schema::V3_SCHEMA)?;
    }

    if version < 4 {
        conn.execute_batch(schema::V4_SCHEMA)?;
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
        assert_eq!(version, "4");

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

    #[test]
    fn v3_migration_adds_proxy_to_schema_tables() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");

        let conn = open_connection(&db_path).unwrap();
        run_migrations(&conn, "test").unwrap();

        // Verify server_schema has proxy column.
        conn.execute(
            "INSERT INTO server_schema (proxy, upstream_url, method, payload, captured_at, schema_hash)
             VALUES ('search', 'http://localhost:9000', 'tools/list', '{}', 1000, 'abc')",
            [],
        )
        .unwrap();

        let proxy: String = conn
            .query_row(
                "SELECT proxy FROM server_schema WHERE upstream_url = 'http://localhost:9000'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(proxy, "search");

        // Verify schema_changes has proxy column.
        conn.execute(
            "INSERT INTO schema_changes (proxy, upstream_url, method, change_type, detected_at)
             VALUES ('search', 'http://localhost:9000', 'tools/list', 'initial', 1000)",
            [],
        )
        .unwrap();

        let proxy: String = conn
            .query_row(
                "SELECT proxy FROM schema_changes WHERE upstream_url = 'http://localhost:9000'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(proxy, "search");

        // Verify UNIQUE(proxy, upstream_url, method) — same upstream+method but different proxy works.
        conn.execute(
            "INSERT INTO server_schema (proxy, upstream_url, method, payload, captured_at, schema_hash)
             VALUES ('email', 'http://localhost:9000', 'tools/list', '{}', 2000, 'def')",
            [],
        )
        .unwrap();

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM server_schema", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 2, "two rows with same upstream but different proxy");
    }

    #[test]
    fn v4_migration_renames_latency_column() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");

        let conn = open_connection(&db_path).unwrap();
        run_migrations(&conn, "test").unwrap();

        // After V4 migration, the column should be latency_us.
        conn.execute(
            "INSERT INTO requests (request_id, ts, proxy, method, latency_us, status)
             VALUES ('r1', 1000, 'api', 'tools/call', 142000, 'ok')",
            [],
        )
        .unwrap();

        let latency: i64 = conn
            .query_row(
                "SELECT latency_us FROM requests WHERE request_id = 'r1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(latency, 142_000);
    }

    #[test]
    fn v4_migration_converts_existing_ms_to_us() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");

        let conn = open_connection(&db_path).unwrap();

        // Simulate V3 state: run V1+V2+V3 only.
        conn.execute_batch(schema::V1_SCHEMA).unwrap();
        conn.execute_batch(schema::V1_META_SEED).unwrap();
        conn.execute_batch(schema::V2_SCHEMA).unwrap();
        conn.execute_batch(schema::V3_SCHEMA).unwrap();

        // Insert data with old ms column.
        conn.execute(
            "INSERT INTO requests (request_id, ts, proxy, method, latency_ms, status)
             VALUES ('r1', 1000, 'api', 'tools/call', 42, 'ok')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO requests (request_id, ts, proxy, method, latency_ms, status)
             VALUES ('r2', 2000, 'api', 'tools/call', 1500, 'ok')",
            [],
        )
        .unwrap();

        // Run V4 migration.
        conn.execute_batch(schema::V4_SCHEMA).unwrap();

        // Verify column renamed and values multiplied by 1000.
        let latency1: i64 = conn
            .query_row(
                "SELECT latency_us FROM requests WHERE request_id = 'r1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(latency1, 42_000, "42ms should become 42,000μs");

        let latency2: i64 = conn
            .query_row(
                "SELECT latency_us FROM requests WHERE request_id = 'r2'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(latency2, 1_500_000, "1500ms should become 1,500,000μs");
    }

    #[test]
    fn v4_migration_rebuilds_slow_index() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");

        let conn = open_connection(&db_path).unwrap();
        run_migrations(&conn, "test").unwrap();

        // Verify the slow index references latency_us.
        let idx_sql: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'index' AND name = 'idx_requests_slow'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            idx_sql.contains("latency_us"),
            "slow index should reference latency_us, got: {idx_sql}"
        );
    }

    #[test]
    fn v3_migration_default_proxy_for_existing_data() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");

        let conn = open_connection(&db_path).unwrap();

        // Simulate V2 state: run V1+V2 only.
        conn.execute_batch(super::schema::V1_SCHEMA).unwrap();
        conn.execute_batch(super::schema::V1_META_SEED).unwrap();
        conn.execute_batch(super::schema::V2_SCHEMA).unwrap();

        // Insert data in V2 schema (no proxy column).
        conn.execute(
            "INSERT INTO server_schema (upstream_url, method, payload, captured_at, schema_hash)
             VALUES ('http://localhost:9000', 'tools/list', '{}', 1000, 'abc')",
            [],
        )
        .unwrap();

        // Run V3 migration.
        conn.execute_batch(super::schema::V3_SCHEMA).unwrap();

        // Existing row should have proxy = 'default'.
        let proxy: String = conn
            .query_row(
                "SELECT proxy FROM server_schema WHERE upstream_url = 'http://localhost:9000'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            proxy, "default",
            "V3 migration should default proxy to 'default'"
        );
    }
}
