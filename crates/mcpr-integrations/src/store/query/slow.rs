//! Query: `mcpr proxy slow <proxy>` — slowest requests above a threshold.

use rusqlite::params;

use super::QueryEngine;
use super::logs::LogRow;

/// Filter parameters for the slow query.
pub struct SlowParams {
    /// Proxy name to filter by.
    pub proxy: String,
    /// Minimum latency in milliseconds to include.
    pub threshold_ms: i64,
    /// Only rows newer than this unix ms timestamp.
    pub since_ts: i64,
    /// Maximum number of rows to return.
    pub limit: i64,
}

impl QueryEngine {
    /// Fetch the slowest requests above a latency threshold, slowest first.
    ///
    /// Reuses [`LogRow`] since the columns are the same — just different
    /// ordering and filtering.
    pub fn slow(&self, params: &SlowParams) -> Result<Vec<LogRow>, rusqlite::Error> {
        let sql = "
            SELECT request_id, ts, method, tool, latency_ms, status,
                   error_msg, session_id, bytes_in, bytes_out
            FROM requests
            WHERE proxy = ?1
              AND latency_ms >= ?2
              AND ts >= ?3
            ORDER BY latency_ms DESC
            LIMIT ?4
        ";

        let mut stmt = self.conn().prepare(sql)?;
        let rows = stmt.query_map(
            params![
                params.proxy,
                params.threshold_ms,
                params.since_ts,
                params.limit,
            ],
            |row| {
                Ok(LogRow {
                    request_id: row.get(0)?,
                    ts: row.get(1)?,
                    method: row.get(2)?,
                    tool: row.get(3)?,
                    latency_ms: row.get(4)?,
                    status: row.get(5)?,
                    error_msg: row.get(6)?,
                    session_id: row.get(7)?,
                    bytes_in: row.get(8)?,
                    bytes_out: row.get(9)?,
                })
            },
        )?;

        rows.collect()
    }

    /// Fetch slow calls newer than a given timestamp (for `--tail` live stream).
    pub fn slow_since(
        &self,
        params: &SlowParams,
        after_ts: i64,
    ) -> Result<Vec<LogRow>, rusqlite::Error> {
        let sql = "
            SELECT request_id, ts, method, tool, latency_ms, status,
                   error_msg, session_id, bytes_in, bytes_out
            FROM requests
            WHERE proxy = ?1
              AND latency_ms >= ?2
              AND ts > ?3
            ORDER BY ts ASC
        ";

        let mut stmt = self.conn().prepare(sql)?;
        let rows = stmt.query_map(
            params![params.proxy, params.threshold_ms, after_ts],
            |row| {
                Ok(LogRow {
                    request_id: row.get(0)?,
                    ts: row.get(1)?,
                    method: row.get(2)?,
                    tool: row.get(3)?,
                    latency_ms: row.get(4)?,
                    status: row.get(5)?,
                    error_msg: row.get(6)?,
                    session_id: row.get(7)?,
                    bytes_in: row.get(8)?,
                    bytes_out: row.get(9)?,
                })
            },
        )?;

        rows.collect()
    }
}
