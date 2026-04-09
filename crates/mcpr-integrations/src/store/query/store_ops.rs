//! Query: `mcpr store stats` and `mcpr store vacuum` — operational commands.

use rusqlite::params;
use std::path::Path;

use super::QueryEngine;

/// Database-level stats returned by `mcpr store stats`.
#[derive(Debug)]
pub struct StoreStats {
    /// Total number of request rows.
    pub total_requests: i64,
    /// Total number of session rows.
    pub total_sessions: i64,
    /// Timestamp of the oldest request (unix ms), None if empty.
    pub oldest_ts: Option<i64>,
    /// Timestamp of the newest request (unix ms), None if empty.
    pub newest_ts: Option<i64>,
    /// Number of distinct proxy names.
    pub proxy_count: i64,
    /// Database file size in bytes.
    pub db_file_size: u64,
    /// WAL file size in bytes (0 if not present).
    pub wal_file_size: u64,
}

/// Result of a vacuum operation.
#[derive(Debug)]
pub struct VacuumResult {
    /// Number of request rows deleted.
    pub deleted_requests: u64,
    /// Number of orphaned session rows deleted.
    pub deleted_sessions: u64,
    /// Whether this was a dry run (no actual deletion).
    pub dry_run: bool,
}

/// Parameters for the vacuum operation.
pub struct VacuumParams {
    /// Delete requests older than this unix ms timestamp.
    pub before_ts: i64,
    /// Optionally scope to a single proxy.
    pub proxy: Option<String>,
    /// If true, report what would be deleted without actually deleting.
    pub dry_run: bool,
}

impl QueryEngine {
    /// Get database-level statistics.
    pub fn store_stats(&self, db_path: &Path) -> Result<StoreStats, rusqlite::Error> {
        let row = self.conn().query_row(
            "SELECT
                COUNT(*),
                MIN(ts),
                MAX(ts),
                COUNT(DISTINCT proxy)
            FROM requests",
            [],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Option<i64>>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            },
        )?;

        let total_sessions: i64 =
            self.conn()
                .query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))?;

        // File sizes from filesystem.
        let db_file_size = std::fs::metadata(db_path).map(|m| m.len()).unwrap_or(0);
        let wal_path = db_path.with_extension("db-wal");
        let wal_file_size = std::fs::metadata(wal_path).map(|m| m.len()).unwrap_or(0);

        Ok(StoreStats {
            total_requests: row.0,
            total_sessions,
            oldest_ts: row.1,
            newest_ts: row.2,
            proxy_count: row.3,
            db_file_size,
            wal_file_size,
        })
    }

    /// Delete old requests and orphaned sessions, optionally scoped to one proxy.
    ///
    /// In dry-run mode, returns the count of rows that would be deleted
    /// without actually deleting anything.
    pub fn vacuum(&self, params: &VacuumParams) -> Result<VacuumResult, rusqlite::Error> {
        if params.dry_run {
            return self.vacuum_dry_run(params);
        }

        // Delete old requests.
        let deleted_requests = if let Some(ref proxy) = params.proxy {
            self.conn().execute(
                "DELETE FROM requests WHERE ts < ?1 AND proxy = ?2",
                rusqlite::params![params.before_ts, proxy],
            )?
        } else {
            self.conn().execute(
                "DELETE FROM requests WHERE ts < ?1",
                params![params.before_ts],
            )?
        };

        // Delete sessions that have no remaining requests and ended before the cutoff.
        let deleted_sessions = self.conn().execute(
            "DELETE FROM sessions
             WHERE session_id NOT IN (SELECT DISTINCT session_id FROM requests WHERE session_id IS NOT NULL)
               AND (ended_at IS NOT NULL AND ended_at < ?1)",
            params![params.before_ts],
        )?;

        // Reclaim disk space.
        self.conn().execute_batch("VACUUM;")?;
        self.conn()
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;

        Ok(VacuumResult {
            deleted_requests: deleted_requests as u64,
            deleted_sessions: deleted_sessions as u64,
            dry_run: false,
        })
    }

    fn vacuum_dry_run(&self, params: &VacuumParams) -> Result<VacuumResult, rusqlite::Error> {
        let deleted_requests: i64 = if let Some(ref proxy) = params.proxy {
            self.conn().query_row(
                "SELECT COUNT(*) FROM requests WHERE ts < ?1 AND proxy = ?2",
                rusqlite::params![params.before_ts, proxy],
                |row| row.get(0),
            )?
        } else {
            self.conn().query_row(
                "SELECT COUNT(*) FROM requests WHERE ts < ?1",
                params![params.before_ts],
                |row| row.get(0),
            )?
        };

        let deleted_sessions: i64 = self.conn().query_row(
            "SELECT COUNT(*) FROM sessions
             WHERE session_id NOT IN (SELECT DISTINCT session_id FROM requests WHERE session_id IS NOT NULL)
               AND (ended_at IS NOT NULL AND ended_at < ?1)",
            params![params.before_ts],
            |row| row.get(0),
        )?;

        Ok(VacuumResult {
            deleted_requests: deleted_requests as u64,
            deleted_sessions: deleted_sessions as u64,
            dry_run: true,
        })
    }
}
