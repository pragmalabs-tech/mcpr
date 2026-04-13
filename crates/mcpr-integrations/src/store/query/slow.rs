//! Query: `mcpr proxy slow` — slowest requests above a threshold.

use rusqlite::params;

use super::QueryEngine;
use super::logs::{LOG_COLUMNS, LogRow, map_log_row};

/// Filter parameters for the slow query.
pub struct SlowParams {
    /// Proxy name to filter by (None = all proxies).
    pub proxy: Option<String>,
    /// Minimum latency in microseconds to include.
    pub threshold_us: i64,
    /// Only rows newer than this unix ms timestamp.
    pub since_ts: i64,
    /// Filter to a specific tool name.
    pub tool: Option<String>,
    /// Maximum number of rows to return.
    pub limit: i64,
}

impl QueryEngine {
    /// Fetch the slowest requests above a latency threshold, slowest first.
    pub fn slow(&self, params: &SlowParams) -> Result<Vec<LogRow>, rusqlite::Error> {
        let sql = format!(
            "SELECT {LOG_COLUMNS}
            FROM requests
            WHERE (?1 IS NULL OR proxy = ?1)
              AND latency_us >= ?2
              AND (?3 IS NULL OR tool = ?3)
              AND ts >= ?4
            ORDER BY latency_us DESC
            LIMIT ?5"
        );

        let mut stmt = self.conn().prepare(&sql)?;
        let rows = stmt.query_map(
            params![
                params.proxy,
                params.threshold_us,
                params.tool,
                params.since_ts,
                params.limit,
            ],
            map_log_row,
        )?;

        rows.collect()
    }

    /// Fetch slow calls newer than a given timestamp (for `--tail` live stream).
    pub fn slow_since(
        &self,
        params: &SlowParams,
        after_ts: i64,
    ) -> Result<Vec<LogRow>, rusqlite::Error> {
        let sql = format!(
            "SELECT {LOG_COLUMNS}
            FROM requests
            WHERE (?1 IS NULL OR proxy = ?1)
              AND latency_us >= ?2
              AND (?3 IS NULL OR tool = ?3)
              AND ts > ?4
            ORDER BY ts ASC"
        );

        let mut stmt = self.conn().prepare(&sql)?;
        let rows = stmt.query_map(
            params![params.proxy, params.threshold_us, params.tool, after_ts],
            map_log_row,
        )?;

        rows.collect()
    }
}
