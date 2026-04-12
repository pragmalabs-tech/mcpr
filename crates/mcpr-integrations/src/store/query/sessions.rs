//! Query: `mcpr proxy sessions <proxy>` — list MCP sessions with client info.

use rusqlite::params;
use serde::Serialize;

use super::QueryEngine;

/// How long since last activity before a session is considered inactive.
/// Used by the `--active` filter and the `is_active` field.
const ACTIVE_SESSION_THRESHOLD_MS: i64 = 5 * 60 * 1000; // 5 minutes

/// Filter parameters for the sessions query.
pub struct SessionsParams {
    /// Proxy name to filter by (None = all proxies).
    pub proxy: Option<String>,
    /// Only sessions started after this unix ms timestamp.
    pub since_ts: i64,
    /// Maximum number of rows to return.
    pub limit: i64,
    /// Only show active sessions (seen within the active threshold).
    pub active_only: bool,
    /// Filter by client name (e.g., "claude-desktop").
    pub client: Option<String>,
}

/// A single session row.
#[derive(Debug, Clone, Serialize)]
pub struct SessionRow {
    pub session_id: String,
    pub client_name: Option<String>,
    pub client_version: Option<String>,
    pub client_platform: Option<String>,
    pub started_at: i64,
    pub last_seen_at: i64,
    pub ended_at: Option<i64>,
    pub total_calls: i64,
    pub total_errors: i64,
    pub is_active: bool,
}

impl QueryEngine {
    /// List sessions for a proxy, most recently seen first.
    pub fn sessions(&self, params: &SessionsParams) -> Result<Vec<SessionRow>, rusqlite::Error> {
        let now_ms = chrono::Utc::now().timestamp_millis();
        let active_threshold = now_ms - ACTIVE_SESSION_THRESHOLD_MS;

        let sql = "
            SELECT
                session_id, client_name, client_version, client_platform,
                started_at, last_seen_at, ended_at, total_calls, total_errors,
                (ended_at IS NULL AND last_seen_at > ?1) AS is_active
            FROM sessions
            WHERE (?2 IS NULL OR proxy = ?2)
              AND (?3 IS NULL OR client_name = ?3)
              AND (?4 = 0 OR (ended_at IS NULL AND last_seen_at > ?1))
              AND started_at >= ?5
            ORDER BY last_seen_at DESC
            LIMIT ?6
        ";

        let active_flag: i64 = if params.active_only { 1 } else { 0 };

        let mut stmt = self.conn().prepare(sql)?;
        let rows = stmt.query_map(
            params![
                active_threshold,
                params.proxy,
                params.client,
                active_flag,
                params.since_ts,
                params.limit,
            ],
            |row| {
                Ok(SessionRow {
                    session_id: row.get(0)?,
                    client_name: row.get(1)?,
                    client_version: row.get(2)?,
                    client_platform: row.get(3)?,
                    started_at: row.get(4)?,
                    last_seen_at: row.get(5)?,
                    ended_at: row.get(6)?,
                    total_calls: row.get(7)?,
                    total_errors: row.get(8)?,
                    is_active: row.get(9)?,
                })
            },
        )?;

        rows.collect()
    }
}
