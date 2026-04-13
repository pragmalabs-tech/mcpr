//! Query: `mcpr proxy logs <proxy>` — recent request log with filtering.

use rusqlite::{Row, params};
use serde::Serialize;

use super::QueryEngine;

/// Filter parameters for the logs query.
pub struct LogsParams {
    /// Proxy name to filter by (None = all proxies).
    pub proxy: Option<String>,
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
    /// Filter by JSON-RPC error code (e.g., "-32601").
    pub error_code: Option<String>,
}

/// A single row from the logs/slow query.
#[derive(Debug, Clone, Serialize)]
pub struct LogRow {
    pub request_id: String,
    pub ts: i64,
    pub method: String,
    pub tool: Option<String>,
    pub latency_us: i64,
    pub status: String,
    pub error_code: Option<String>,
    pub error_msg: Option<String>,
    pub session_id: Option<String>,
    pub bytes_in: Option<i64>,
    pub bytes_out: Option<i64>,
}

/// Shared row mapper — used by logs, logs_since, slow, slow_since to avoid
/// duplicating the 11-column mapping closure.
pub(crate) fn map_log_row(row: &Row<'_>) -> rusqlite::Result<LogRow> {
    Ok(LogRow {
        request_id: row.get(0)?,
        ts: row.get(1)?,
        method: row.get(2)?,
        tool: row.get(3)?,
        latency_us: row.get(4)?,
        status: row.get(5)?,
        error_code: row.get(6)?,
        error_msg: row.get(7)?,
        session_id: row.get(8)?,
        bytes_in: row.get(9)?,
        bytes_out: row.get(10)?,
    })
}

/// The 11 columns selected in all log/slow queries.
pub(crate) const LOG_COLUMNS: &str = "request_id, ts, method, tool, latency_us, status, error_code, error_msg, session_id, bytes_in, bytes_out";

impl QueryEngine {
    /// Fetch recent request logs, newest first.
    pub fn logs(&self, params: &LogsParams) -> Result<Vec<LogRow>, rusqlite::Error> {
        let sql = format!(
            "SELECT {LOG_COLUMNS}
            FROM requests
            WHERE (?1 IS NULL OR proxy = ?1)
              AND (?2 IS NULL OR tool = ?2)
              AND (?3 IS NULL OR status = ?3)
              AND (?4 IS NULL OR method = ?4)
              AND (?5 IS NULL OR session_id LIKE ?5 || '%')
              AND (?6 IS NULL OR error_code = ?6)
              AND ts >= ?7
            ORDER BY ts DESC
            LIMIT ?8"
        );

        let mut stmt = self.conn().prepare(&sql)?;
        let rows = stmt.query_map(
            params![
                params.proxy,
                params.tool,
                params.status,
                params.method,
                params.session,
                params.error_code,
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
            WHERE (?1 IS NULL OR proxy = ?1)
              AND (?2 IS NULL OR tool = ?2)
              AND (?3 IS NULL OR status = ?3)
              AND (?4 IS NULL OR method = ?4)
              AND (?5 IS NULL OR session_id LIKE ?5 || '%')
              AND (?6 IS NULL OR error_code = ?6)
              AND ts > ?7
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
                params.error_code,
                after_ts
            ],
            map_log_row,
        )?;

        rows.collect()
    }
}
