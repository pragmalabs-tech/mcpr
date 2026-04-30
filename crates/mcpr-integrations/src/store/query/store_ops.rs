//! Query: `mcpr store stats` and `mcpr store vacuum` — operational commands.

use rusqlite::params;
use serde::Serialize;
use std::path::Path;

use super::QueryEngine;

/// Database-level stats returned by `mcpr store stats`.
#[derive(Debug, Serialize)]
pub struct StoreStats {
    pub total_requests: i64,
    pub total_sessions: i64,
    pub oldest_ts: Option<i64>,
    pub newest_ts: Option<i64>,
    pub proxy_count: i64,
    pub db_file_size: u64,
    pub wal_file_size: u64,
}

/// Result of a vacuum operation.
#[derive(Debug, Serialize)]
pub struct VacuumResult {
    pub deleted_requests: u64,
    pub deleted_sessions: u64,
    pub dry_run: bool,
}

/// Parameters for the vacuum operation.
pub struct VacuumParams {
    pub before_ts: i64,
    pub proxy: Option<String>,
    pub dry_run: bool,
}

/// Count rows matching the vacuum filter (shared by vacuum and dry-run).
fn count_matching_requests(
    conn: &rusqlite::Connection,
    before_ts: i64,
    proxy: Option<&str>,
) -> rusqlite::Result<i64> {
    if let Some(proxy) = proxy {
        conn.query_row(
            "SELECT COUNT(*) FROM requests WHERE ts < ?1 AND proxy = ?2",
            params![before_ts, proxy],
            |row| row.get(0),
        )
    } else {
        conn.query_row(
            "SELECT COUNT(*) FROM requests WHERE ts < ?1",
            params![before_ts],
            |row| row.get(0),
        )
    }
}

/// Count orphaned sessions matching the vacuum filter. A session is
/// orphaned if it has no remaining requests (we just deleted the old
/// ones in the same vacuum) and was already closed before `before_ts`.
fn count_orphaned_sessions(conn: &rusqlite::Connection, before_ts: i64) -> rusqlite::Result<i64> {
    conn.query_row(
        "SELECT COUNT(*) FROM sessions
         WHERE id NOT IN (SELECT DISTINCT session_id FROM requests WHERE session_id IS NOT NULL)
           AND state = 'closed'
           AND last_active < ?1",
        params![before_ts],
        |row| row.get(0),
    )
}

impl QueryEngine {
    /// Get database-level statistics.
    pub fn store_stats(&self, db_path: &Path) -> Result<StoreStats, rusqlite::Error> {
        let row = self.conn().query_row(
            "SELECT COUNT(*), MIN(ts), MAX(ts), COUNT(DISTINCT proxy) FROM requests",
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
                .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get(0))?;

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
    /// In dry-run mode, returns counts without deleting.
    pub fn vacuum(&self, params: &VacuumParams) -> Result<VacuumResult, rusqlite::Error> {
        if params.dry_run {
            let deleted_requests =
                count_matching_requests(self.conn(), params.before_ts, params.proxy.as_deref())?;
            let deleted_sessions = count_orphaned_sessions(self.conn(), params.before_ts)?;
            return Ok(VacuumResult {
                deleted_requests: deleted_requests as u64,
                deleted_sessions: deleted_sessions as u64,
                dry_run: true,
            });
        }

        // Delete old requests.
        let deleted_requests = if let Some(ref proxy) = params.proxy {
            self.conn().execute(
                "DELETE FROM requests WHERE ts < ?1 AND proxy = ?2",
                params![params.before_ts, proxy],
            )?
        } else {
            self.conn().execute(
                "DELETE FROM requests WHERE ts < ?1",
                params![params.before_ts],
            )?
        };

        // Delete orphaned sessions.
        let deleted_sessions = self.conn().execute(
            "DELETE FROM sessions
             WHERE id NOT IN (SELECT DISTINCT session_id FROM requests WHERE session_id IS NOT NULL)
               AND state = 'closed'
               AND last_active < ?1",
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
}
