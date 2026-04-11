//! Query engine — read-only access to the storage database.
//!
//! All CLI observability commands (`mcpr proxy logs`, `mcpr proxy slow`, etc.)
//! are thin wrappers around [`QueryEngine`] methods. Each method executes a
//! parameterized SQL query and maps rows to typed result structs.
//!
//! The query engine opens its own read-only connection to the database.
//! WAL mode ensures this never blocks the background writer.

pub mod clients;
pub mod logs;
pub mod session_detail;
pub mod sessions;
pub mod slow;
pub mod stats;
pub mod store_ops;

use rusqlite::Connection;
use std::path::Path;

use super::db;

/// Read-only query interface to the storage database.
pub struct QueryEngine {
    conn: Connection,
}

impl QueryEngine {
    /// Open a query connection to the database at the given path.
    pub fn open(db_path: &Path) -> Result<Self, rusqlite::Error> {
        let conn = db::open_connection(db_path)?;
        Ok(QueryEngine { conn })
    }

    /// Get a reference to the underlying connection (for query methods).
    pub(crate) fn conn(&self) -> &Connection {
        &self.conn
    }

    /// Create a query engine from an in-memory connection (for testing).
    #[cfg(test)]
    pub(crate) fn from_conn(conn: Connection) -> Self {
        QueryEngine { conn }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::db;
    use rusqlite::params;

    /// Create a test QueryEngine with schema applied and seed data inserted.
    pub(crate) fn seeded_engine() -> QueryEngine {
        let conn = Connection::open_in_memory().unwrap();
        db::run_migrations(&conn, "test").unwrap();

        // Seed sessions
        conn.execute(
            "INSERT INTO sessions (session_id, proxy, started_at, last_seen_at, client_name, client_version, client_platform, total_calls, total_errors)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params!["s1", "api", 1000, 5000, "claude-desktop", "1.2.0", "claude", 3, 1],
        ).unwrap();
        conn.execute(
            "INSERT INTO sessions (session_id, proxy, started_at, last_seen_at, ended_at, client_name, client_version, client_platform, total_calls, total_errors)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params!["s2", "api", 2000, 3000, 3500, "cursor", "0.44", "cursor", 2, 0],
        ).unwrap();

        // Seed requests
        let requests = vec![
            (
                "r1",
                1000i64,
                "api",
                Some("s1"),
                "tools/call",
                Some("search"),
                142i64,
                "ok",
                None::<&str>,
                None::<&str>,
                Some(256i64),
                Some(1024i64),
            ),
            (
                "r2",
                2000,
                "api",
                Some("s1"),
                "tools/call",
                Some("search"),
                891,
                "ok",
                None,
                None,
                Some(256),
                Some(4096),
            ),
            (
                "r3",
                3000,
                "api",
                Some("s1"),
                "tools/call",
                Some("create_order"),
                4201,
                "error",
                Some("-32600"),
                Some("timeout"),
                Some(512),
                None,
            ),
            (
                "r4",
                4000,
                "api",
                Some("s2"),
                "resources/read",
                None,
                23,
                "ok",
                None,
                None,
                Some(64),
                Some(2048),
            ),
            (
                "r5",
                5000,
                "api",
                Some("s2"),
                "tools/call",
                Some("search"),
                156,
                "ok",
                None,
                None,
                Some(256),
                Some(1024),
            ),
        ];

        for (
            id,
            ts,
            proxy,
            sid,
            method,
            tool,
            latency,
            status,
            err_code,
            err_msg,
            bytes_in,
            bytes_out,
        ) in requests
        {
            conn.execute(
                "INSERT INTO requests (request_id, ts, proxy, session_id, method, tool, latency_ms, status, error_code, error_msg, bytes_in, bytes_out)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                params![id, ts, proxy, sid, method, tool, latency, status, err_code, err_msg, bytes_in, bytes_out],
            ).unwrap();
        }

        QueryEngine::from_conn(conn)
    }

    // ── logs ────────────────────────────────────────────────────────────

    #[test]
    fn logs_returns_all_rows() {
        let engine = seeded_engine();
        let rows = engine
            .logs(&super::logs::LogsParams {
                proxy: "api".into(),
                since_ts: 0,
                limit: 100,
                tool: None,
                method: None,
                session: None,
                status: None,
                error_code: None,
            })
            .unwrap();
        assert_eq!(rows.len(), 5);
        // Newest first
        assert_eq!(rows[0].request_id, "r5");
        assert_eq!(rows[4].request_id, "r1");
    }

    #[test]
    fn logs_filter_by_tool() {
        let engine = seeded_engine();
        let rows = engine
            .logs(&super::logs::LogsParams {
                proxy: "api".into(),
                since_ts: 0,
                limit: 100,
                tool: Some("search".into()),
                method: None,
                session: None,
                status: None,
                error_code: None,
            })
            .unwrap();
        assert_eq!(rows.len(), 3);
        assert!(rows.iter().all(|r| r.tool.as_deref() == Some("search")));
    }

    #[test]
    fn logs_filter_by_status() {
        let engine = seeded_engine();
        let rows = engine
            .logs(&super::logs::LogsParams {
                proxy: "api".into(),
                since_ts: 0,
                limit: 100,
                tool: None,
                method: None,
                session: None,
                status: Some("error".into()),
                error_code: None,
            })
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].request_id, "r3");
        assert_eq!(rows[0].error_msg.as_deref(), Some("timeout"));
    }

    #[test]
    fn logs_since_returns_newer_rows() {
        let engine = seeded_engine();
        let params = super::logs::LogsParams {
            proxy: "api".into(),
            since_ts: 0,
            limit: 100,
            tool: None,
            method: None,
            session: None,
            status: None,
            error_code: None,
        };
        let rows = engine.logs_since(&params, 3000).unwrap();
        assert_eq!(rows.len(), 2);
        // Oldest first
        assert_eq!(rows[0].request_id, "r4");
        assert_eq!(rows[1].request_id, "r5");
    }

    #[test]
    fn logs_empty_proxy() {
        let engine = seeded_engine();
        let rows = engine
            .logs(&super::logs::LogsParams {
                proxy: "nonexistent".into(),
                since_ts: 0,
                limit: 100,
                tool: None,
                method: None,
                session: None,
                status: None,
                error_code: None,
            })
            .unwrap();
        assert!(rows.is_empty());
    }

    #[test]
    fn logs_filter_by_session() {
        let engine = seeded_engine();
        // s1 has r1, r2, r3
        let rows = engine
            .logs(&super::logs::LogsParams {
                proxy: "api".into(),
                since_ts: 0,
                limit: 100,
                tool: None,
                method: None,
                session: Some("s1".into()),
                status: None,
                error_code: None,
            })
            .unwrap();
        assert_eq!(rows.len(), 3);
        assert!(rows.iter().all(|r| r.session_id.as_deref() == Some("s1")));
    }

    #[test]
    fn logs_filter_by_session_prefix() {
        let engine = seeded_engine();
        // "s" matches both s1 and s2 — all 5 rows
        let rows = engine
            .logs(&super::logs::LogsParams {
                proxy: "api".into(),
                since_ts: 0,
                limit: 100,
                tool: None,
                method: None,
                session: Some("s".into()),
                status: None,
                error_code: None,
            })
            .unwrap();
        assert_eq!(rows.len(), 5);
    }

    #[test]
    fn logs_filter_by_method() {
        let engine = seeded_engine();
        // resources/read: only r4
        let rows = engine
            .logs(&super::logs::LogsParams {
                proxy: "api".into(),
                since_ts: 0,
                limit: 100,
                tool: None,
                method: Some("resources/read".into()),
                session: None,
                status: None,
                error_code: None,
            })
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].request_id, "r4");
    }

    #[test]
    fn logs_filter_combined_session_and_method() {
        let engine = seeded_engine();
        // s1 + tools/call = r1, r2, r3
        let rows = engine
            .logs(&super::logs::LogsParams {
                proxy: "api".into(),
                since_ts: 0,
                limit: 100,
                tool: None,
                method: Some("tools/call".into()),
                session: Some("s1".into()),
                status: None,
                error_code: None,
            })
            .unwrap();
        assert_eq!(rows.len(), 3);
    }

    // ── slow ────────────────────────────────────────────────────────────

    #[test]
    fn slow_filter_by_tool() {
        let engine = seeded_engine();
        // search has latencies 42, 891, 156 — only 891 is above 500
        let rows = engine
            .slow(&super::slow::SlowParams {
                proxy: "api".into(),
                tool: Some("search".into()),
                threshold_ms: 500,
                since_ts: 0,
                limit: 100,
            })
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].tool.as_deref(), Some("search"));
        assert_eq!(rows[0].latency_ms, 891);
    }

    #[test]
    fn slow_returns_above_threshold() {
        let engine = seeded_engine();
        let rows = engine
            .slow(&super::slow::SlowParams {
                proxy: "api".into(),
                tool: None,
                threshold_ms: 500,
                since_ts: 0,
                limit: 100,
            })
            .unwrap();
        assert_eq!(rows.len(), 2);
        // Slowest first
        assert_eq!(rows[0].latency_ms, 4201);
        assert_eq!(rows[1].latency_ms, 891);
    }

    #[test]
    fn slow_high_threshold_returns_empty() {
        let engine = seeded_engine();
        let rows = engine
            .slow(&super::slow::SlowParams {
                proxy: "api".into(),
                tool: None,
                threshold_ms: 10000,
                since_ts: 0,
                limit: 100,
            })
            .unwrap();
        assert!(rows.is_empty());
    }

    // ── stats ───────────────────────────────────────────────────────────

    #[test]
    fn stats_aggregates_correctly() {
        let engine = seeded_engine();
        let result = engine
            .stats(&super::stats::StatsParams {
                proxy: "api".into(),
                since_ts: 0,
            })
            .unwrap();
        assert_eq!(result.total_calls, 5);
        assert!(result.error_pct > 0.0); // 1 error out of 5
        // search tool should have 3 calls
        let search = result.tools.iter().find(|t| t.label == "search").unwrap();
        assert_eq!(search.calls, 3);
    }

    #[test]
    fn stats_empty_proxy() {
        let engine = seeded_engine();
        let result = engine
            .stats(&super::stats::StatsParams {
                proxy: "nonexistent".into(),
                since_ts: 0,
            })
            .unwrap();
        assert_eq!(result.total_calls, 0);
        assert!(result.tools.is_empty());
    }

    // ── clients ─────────────────────────────────────────────────────────

    #[test]
    fn clients_aggregates_by_client() {
        let engine = seeded_engine();
        let rows = engine
            .clients(&super::clients::ClientsParams {
                proxy: "api".into(),
                since_ts: 0,
            })
            .unwrap();
        assert_eq!(rows.len(), 2);
        // Sorted by total_calls desc
        assert_eq!(rows[0].client_name.as_deref(), Some("claude-desktop"));
        assert_eq!(rows[0].total_calls, 3);
        assert_eq!(rows[1].client_name.as_deref(), Some("cursor"));
        assert_eq!(rows[1].total_calls, 2);
    }

    // ── sessions ────────────────────────────────────────────────────────

    #[test]
    fn sessions_returns_all() {
        let engine = seeded_engine();
        let rows = engine
            .sessions(&super::sessions::SessionsParams {
                proxy: "api".into(),
                since_ts: 0,
                limit: 100,
                active_only: false,
                client: None,
            })
            .unwrap();
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn sessions_filter_by_client() {
        let engine = seeded_engine();
        let rows = engine
            .sessions(&super::sessions::SessionsParams {
                proxy: "api".into(),
                since_ts: 0,
                limit: 100,
                active_only: false,
                client: Some("cursor".into()),
            })
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].client_name.as_deref(), Some("cursor"));
    }

    // ── session_detail ──────────────────────────────────────────────────

    #[test]
    fn session_detail_returns_session_with_requests() {
        let engine = seeded_engine();
        let detail = engine.session_detail("s1").unwrap().unwrap();
        assert_eq!(detail.session_id, "s1");
        assert_eq!(detail.client_name.as_deref(), Some("claude-desktop"));
        assert_eq!(detail.client_version.as_deref(), Some("1.2.0"));
        assert_eq!(detail.client_platform.as_deref(), Some("claude"));
        assert_eq!(detail.total_calls, 3);
        assert_eq!(detail.total_errors, 1);
        // 3 requests belong to s1 (r1, r2, r3), oldest first
        assert_eq!(detail.requests.len(), 3);
        assert_eq!(detail.requests[0].request_id, "r1");
        assert_eq!(detail.requests[1].request_id, "r2");
        assert_eq!(detail.requests[2].request_id, "r3");
    }

    #[test]
    fn session_detail_closed_session() {
        let engine = seeded_engine();
        let detail = engine.session_detail("s2").unwrap().unwrap();
        assert_eq!(detail.session_id, "s2");
        assert_eq!(detail.client_name.as_deref(), Some("cursor"));
        assert_eq!(detail.ended_at, Some(3500));
        // 2 requests belong to s2 (r4, r5), oldest first
        assert_eq!(detail.requests.len(), 2);
        assert_eq!(detail.requests[0].request_id, "r4");
        assert_eq!(detail.requests[1].request_id, "r5");
    }

    #[test]
    fn session_detail_nonexistent_returns_none() {
        let engine = seeded_engine();
        let result = engine.session_detail("no-such-session").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn session_detail_requests_ordered_oldest_first() {
        let engine = seeded_engine();
        let detail = engine.session_detail("s1").unwrap().unwrap();
        for pair in detail.requests.windows(2) {
            assert!(
                pair[0].ts <= pair[1].ts,
                "requests should be ordered by ts ASC"
            );
        }
    }

    #[test]
    fn session_detail_serializes_to_json() {
        let engine = seeded_engine();
        let detail = engine.session_detail("s1").unwrap().unwrap();
        let json = serde_json::to_string(&detail).unwrap();
        assert!(json.contains("session_id"));
        assert!(json.contains("client_name"));
        assert!(json.contains("requests"));
        assert!(json.contains("r1"));
    }

    // ── store_ops ───────────────────────────────────────────────────────

    #[test]
    fn vacuum_dry_run_counts_correctly() {
        let engine = seeded_engine();
        let result = engine
            .vacuum(&super::store_ops::VacuumParams {
                before_ts: 3500,
                proxy: None,
                dry_run: true,
            })
            .unwrap();
        // r1 (ts=1000), r2 (ts=2000), r3 (ts=3000) are before 3500
        assert_eq!(result.deleted_requests, 3);
        assert!(result.dry_run);
    }

    #[test]
    fn vacuum_actually_deletes() {
        let engine = seeded_engine();
        let result = engine
            .vacuum(&super::store_ops::VacuumParams {
                before_ts: 3500,
                proxy: None,
                dry_run: false,
            })
            .unwrap();
        assert_eq!(result.deleted_requests, 3);
        assert!(!result.dry_run);

        // Verify remaining rows
        let remaining = engine
            .logs(&super::logs::LogsParams {
                proxy: "api".into(),
                since_ts: 0,
                limit: 100,
                tool: None,
                method: None,
                session: None,
                status: None,
                error_code: None,
            })
            .unwrap();
        assert_eq!(remaining.len(), 2);
    }

    // ── serialization ───────────────────────────────────────────────────

    #[test]
    fn log_row_serializes_to_json() {
        let engine = seeded_engine();
        let rows = engine
            .logs(&super::logs::LogsParams {
                proxy: "api".into(),
                since_ts: 0,
                limit: 1,
                tool: None,
                method: None,
                session: None,
                status: None,
                error_code: None,
            })
            .unwrap();
        let json = serde_json::to_string(&rows[0]).unwrap();
        assert!(json.contains("request_id"));
        assert!(json.contains("latency_ms"));
    }

    #[test]
    fn client_row_serializes_to_json() {
        let engine = seeded_engine();
        let rows = engine
            .clients(&super::clients::ClientsParams {
                proxy: "api".into(),
                since_ts: 0,
            })
            .unwrap();
        let json = serde_json::to_string(&rows[0]).unwrap();
        assert!(json.contains("client_name"));
        assert!(json.contains("total_calls"));
    }

    #[test]
    fn stats_result_serializes_to_json() {
        let engine = seeded_engine();
        let result = engine
            .stats(&super::stats::StatsParams {
                proxy: "api".into(),
                since_ts: 0,
            })
            .unwrap();
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("total_calls"));
        assert!(json.contains("tools"));
    }
}
