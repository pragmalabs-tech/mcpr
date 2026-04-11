//! Query: `mcpr proxy logs <proxy>` — recent request log with filtering.

use rusqlite::{Row, params};
use serde::Serialize;

use super::QueryEngine;

/// Filter parameters for the logs query.
pub struct LogsParams {
    /// Proxy name to filter by.
    pub proxy: String,
    /// Only rows newer than this unix ms timestamp.
    pub since_ts: i64,
    /// Maximum number of rows to return.
    pub limit: i64,
    /// Filter to a specific tool name.
    pub tool: Option<String>,
    /// Filter by MCP method (e.g., "tools/call", "resources/read").
    pub method: Option<String>,
    /// Filter by session ID.
    pub session: Option<String>,
    /// Filter by status ("ok", "error", "timeout").
    pub status: Option<String>,
}

/// A single row from the logs/slow query.
#[derive(Debug, Clone, Serialize)]
pub struct LogRow {
    pub request_id: String,
    pub ts: i64,
    pub method: String,
    pub tool: Option<String>,
    pub latency_ms: i64,
    pub status: String,
    pub error_msg: Option<String>,
    pub session_id: Option<String>,
    pub bytes_in: Option<i64>,
    pub bytes_out: Option<i64>,
}

/// Shared row mapper — used by logs, logs_since, slow, slow_since to avoid
/// duplicating the 10-column mapping closure.
pub(crate) fn map_log_row(row: &Row<'_>) -> rusqlite::Result<LogRow> {
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
}

/// The 10 columns selected in all log/slow queries.
pub(crate) const LOG_COLUMNS: &str =
    "request_id, ts, method, tool, latency_ms, status, error_msg, session_id, bytes_in, bytes_out";

impl QueryEngine {
    /// Fetch recent request logs, newest first.
    pub fn logs(&self, params: &LogsParams) -> Result<Vec<LogRow>, rusqlite::Error> {
        let sql = format!(
            "SELECT {LOG_COLUMNS}
            FROM requests
            WHERE proxy = ?1
              AND (?2 IS NULL OR tool = ?2)
              AND (?3 IS NULL OR status = ?3)
              AND (?4 IS NULL OR method = ?4)
              AND (?5 IS NULL OR session_id LIKE ?5 || '%')
              AND ts >= ?6
            ORDER BY ts DESC
            LIMIT ?7"
        );

        let mut stmt = self.conn().prepare(&sql)?;
        let rows = stmt.query_map(
            params![
                params.proxy,
                params.tool,
                params.status,
                params.method,
                params.session,
                params.since_ts,
                params.limit,
            ],
            map_log_row,
        )?;

        rows.collect()
    }

    /// Fetch logs newer than a given timestamp, oldest first.
    ///
    /// Used for `--follow` mode: poll every 500ms with the last seen timestamp.
    pub fn logs_since(
        &self,
        params: &LogsParams,
        after_ts: i64,
    ) -> Result<Vec<LogRow>, rusqlite::Error> {
        let sql = format!(
            "SELECT {LOG_COLUMNS}
            FROM requests
            WHERE proxy = ?1
              AND (?2 IS NULL OR tool = ?2)
              AND (?3 IS NULL OR status = ?3)
              AND (?4 IS NULL OR method = ?4)
              AND (?5 IS NULL OR session_id LIKE ?5 || '%')
              AND ts > ?6
            ORDER BY ts ASC"
        );

        let mut stmt = self.conn().prepare(&sql)?;
        let rows = stmt.query_map(
            params![
                params.proxy,
                params.tool,
                params.status,
                params.method,
                params.session,
                after_ts
            ],
            map_log_row,
        )?;

        rows.collect()
    }
}
