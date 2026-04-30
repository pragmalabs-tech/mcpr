//! Query: `mcpr proxy session <id>` — drill into a single session with its requests.

use rusqlite::params;
use serde::Serialize;

use super::QueryEngine;
use super::logs::{LOG_COLUMNS, LogRow, map_log_row};

/// A session with all its associated request logs.
#[derive(Debug, Serialize)]
pub struct SessionDetail {
    pub session_id: String,
    pub client_name: Option<String>,
    pub client_version: Option<String>,
    pub client_platform: Option<String>,
    pub started_at: i64,
    pub last_seen_at: i64,
    pub ended_at: Option<i64>,
    pub total_calls: i64,
    pub total_errors: i64,
    /// All requests in this session, ordered oldest-first.
    pub requests: Vec<LogRow>,
}

impl QueryEngine {
    /// Fetch a single session by ID, along with all its request logs.
    ///
    /// Returns `None` if the session doesn't exist.
    pub fn session_detail(
        &self,
        session_id: &str,
    ) -> Result<Option<SessionDetail>, rusqlite::Error> {
        // Step 1: fetch the session row (supports prefix matching like git SHAs).
        let session_sql = "
            SELECT
                session_id, client_name, client_version, client_platform,
                started_at, last_seen_at, ended_at, total_calls, total_errors
            FROM sessions_view
            WHERE session_id LIKE ?1 || '%'
        ";

        let session = self
            .conn()
            .query_row(session_sql, params![session_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, Option<i64>>(6)?,
                    row.get::<_, i64>(7)?,
                    row.get::<_, i64>(8)?,
                ))
            });

        let (
            sid,
            client_name,
            client_version,
            client_platform,
            started_at,
            last_seen_at,
            ended_at,
            total_calls,
            total_errors,
        ) = match session {
            Ok(row) => row,
            Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(None),
            Err(e) => return Err(e),
        };

        // Step 2: fetch all requests for this session, oldest first.
        let requests_sql = format!(
            "SELECT {LOG_COLUMNS}
            FROM request_log
            WHERE session_id = ?1
            ORDER BY ts ASC"
        );

        let mut stmt = self.conn().prepare(&requests_sql)?;
        let requests: Vec<LogRow> = stmt
            .query_map(params![&sid], map_log_row)?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(Some(SessionDetail {
            session_id: sid,
            client_name,
            client_version,
            client_platform,
            started_at,
            last_seen_at,
            ended_at,
            total_calls,
            total_errors,
            requests,
        }))
    }
}
