//! Query: `mcpr proxy clients <proxy>` — aggregated client breakdown.

use rusqlite::params;

use super::QueryEngine;

/// Filter parameters for the clients query.
pub struct ClientsParams {
    /// Proxy name to filter by.
    pub proxy: String,
    /// Only sessions started after this unix ms timestamp.
    pub since_ts: i64,
}

/// Aggregated stats for one client identity (name + version + platform).
#[derive(Debug, Clone)]
pub struct ClientRow {
    pub client_name: Option<String>,
    pub client_version: Option<String>,
    pub client_platform: Option<String>,
    /// Number of distinct sessions from this client.
    pub sessions: i64,
    /// Total calls across all sessions.
    pub total_calls: i64,
    /// Total errors across all sessions.
    pub total_errors: i64,
    /// Error percentage (0.0 to 100.0).
    pub error_pct: f64,
    /// Earliest session start time (unix ms).
    pub first_seen: i64,
    /// Most recent activity (unix ms).
    pub last_seen: i64,
}

impl QueryEngine {
    /// Aggregate client usage across sessions, sorted by total calls descending.
    pub fn clients(&self, params: &ClientsParams) -> Result<Vec<ClientRow>, rusqlite::Error> {
        let sql = "
            SELECT
                client_name,
                client_version,
                client_platform,
                COUNT(DISTINCT session_id) AS sessions,
                SUM(total_calls) AS total_calls,
                SUM(total_errors) AS total_errors,
                CASE WHEN SUM(total_calls) > 0
                     THEN SUM(total_errors) * 100.0 / SUM(total_calls)
                     ELSE 0.0
                END AS error_pct,
                MIN(started_at) AS first_seen,
                MAX(last_seen_at) AS last_seen
            FROM sessions
            WHERE proxy = ?1
              AND started_at >= ?2
            GROUP BY client_name, client_version, client_platform
            ORDER BY total_calls DESC
        ";

        let mut stmt = self.conn().prepare(sql)?;
        let rows = stmt.query_map(params![params.proxy, params.since_ts], |row| {
            Ok(ClientRow {
                client_name: row.get(0)?,
                client_version: row.get(1)?,
                client_platform: row.get(2)?,
                sessions: row.get(3)?,
                total_calls: row.get(4)?,
                total_errors: row.get(5)?,
                error_pct: row.get(6)?,
                first_seen: row.get(7)?,
                last_seen: row.get(8)?,
            })
        })?;

        rows.collect()
    }
}
