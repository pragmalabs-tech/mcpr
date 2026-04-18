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
pub mod schema;
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
#[allow(non_snake_case)]
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
                "INSERT INTO requests (request_id, ts, proxy, session_id, method, tool, latency_us, status, error_code, error_msg, bytes_in, bytes_out)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                params![id, ts, proxy, sid, method, tool, latency, status, err_code, err_msg, bytes_in, bytes_out],
            ).unwrap();
        }

        QueryEngine::from_conn(conn)
    }

    // ── logs ────────────────────────────────────────────────────────────

    #[test]
    fn logs__returns_all_rows() {
        let engine = seeded_engine();
        let rows = engine
            .logs(&super::logs::LogsParams {
                proxy: Some("api".into()),
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
        assert_eq!(rows[0].request_id, "r5");
        assert_eq!(rows[4].request_id, "r1");
    }

    #[test]
    fn logs__filter_by_tool() {
        let engine = seeded_engine();
        let rows = engine
            .logs(&super::logs::LogsParams {
                proxy: Some("api".into()),
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
    fn logs__filter_by_status() {
        let engine = seeded_engine();
        let rows = engine
            .logs(&super::logs::LogsParams {
                proxy: Some("api".into()),
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
    fn logs__since_returns_newer() {
        let engine = seeded_engine();
        let params = super::logs::LogsParams {
            proxy: Some("api".into()),
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
        assert_eq!(rows[0].request_id, "r4");
        assert_eq!(rows[1].request_id, "r5");
    }

    #[test]
    fn logs__empty_proxy() {
        let engine = seeded_engine();
        let rows = engine
            .logs(&super::logs::LogsParams {
                proxy: Some("nonexistent".into()),
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
    fn logs__filter_by_session() {
        let engine = seeded_engine();
        let rows = engine
            .logs(&super::logs::LogsParams {
                proxy: Some("api".into()),
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
    fn logs__filter_by_session_prefix() {
        let engine = seeded_engine();
        let rows = engine
            .logs(&super::logs::LogsParams {
                proxy: Some("api".into()),
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
    fn logs__filter_by_method() {
        let engine = seeded_engine();
        let rows = engine
            .logs(&super::logs::LogsParams {
                proxy: Some("api".into()),
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
    fn logs__filter_combined_session_and_method() {
        let engine = seeded_engine();
        let rows = engine
            .logs(&super::logs::LogsParams {
                proxy: Some("api".into()),
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

    #[test]
    fn logs__filter_by_error_code() {
        let engine = seeded_engine();
        let rows = engine
            .logs(&super::logs::LogsParams {
                proxy: Some("api".into()),
                since_ts: 0,
                limit: 100,
                tool: None,
                method: None,
                session: None,
                status: None,
                error_code: Some("-32600".into()),
            })
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].request_id, "r3");
        assert_eq!(rows[0].error_code.as_deref(), Some("-32600"));
    }

    #[test]
    fn logs__filter_by_error_code_no_match() {
        let engine = seeded_engine();
        let rows = engine
            .logs(&super::logs::LogsParams {
                proxy: Some("api".into()),
                since_ts: 0,
                limit: 100,
                tool: None,
                method: None,
                session: None,
                status: None,
                error_code: Some("-32601".into()),
            })
            .unwrap();
        assert!(rows.is_empty());
    }

    #[test]
    fn logs__error_code_present_in_row() {
        let engine = seeded_engine();
        let rows = engine
            .logs(&super::logs::LogsParams {
                proxy: Some("api".into()),
                since_ts: 0,
                limit: 100,
                tool: None,
                method: None,
                session: None,
                status: None,
                error_code: None,
            })
            .unwrap();
        let r3 = rows.iter().find(|r| r.request_id == "r3").unwrap();
        assert_eq!(r3.error_code.as_deref(), Some("-32600"));
        let r1 = rows.iter().find(|r| r.request_id == "r1").unwrap();
        assert!(r1.error_code.is_none());
    }

    // ── slow ────────────────────────────────────────────────────────────

    #[test]
    fn slow__filter_by_tool() {
        let engine = seeded_engine();
        let rows = engine
            .slow(&super::slow::SlowParams {
                proxy: Some("api".into()),
                tool: Some("search".into()),
                threshold_us: 500,
                since_ts: 0,
                limit: 100,
            })
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].tool.as_deref(), Some("search"));
        assert_eq!(rows[0].latency_us, 891);
    }

    #[test]
    fn slow__returns_above_threshold() {
        let engine = seeded_engine();
        let rows = engine
            .slow(&super::slow::SlowParams {
                proxy: Some("api".into()),
                tool: None,
                threshold_us: 500,
                since_ts: 0,
                limit: 100,
            })
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].latency_us, 4201);
        assert_eq!(rows[1].latency_us, 891);
    }

    #[test]
    fn slow__high_threshold_returns_empty() {
        let engine = seeded_engine();
        let rows = engine
            .slow(&super::slow::SlowParams {
                proxy: Some("api".into()),
                tool: None,
                threshold_us: 10000,
                since_ts: 0,
                limit: 100,
            })
            .unwrap();
        assert!(rows.is_empty());
    }

    // ── slow_since (--follow) ──────────────────────────────────────────

    #[test]
    fn slow_since__returns_newer_rows() {
        let engine = seeded_engine();
        let params = super::slow::SlowParams {
            proxy: Some("api".into()),
            threshold_us: 500,
            since_ts: 0,
            tool: None,
            limit: 100,
        };
        let rows = engine.slow_since(&params, 1000).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].request_id, "r2");
        assert_eq!(rows[1].request_id, "r3");
    }

    #[test]
    fn slow_since__excludes_at_boundary() {
        let engine = seeded_engine();
        let params = super::slow::SlowParams {
            proxy: Some("api".into()),
            threshold_us: 500,
            since_ts: 0,
            tool: None,
            limit: 100,
        };
        let rows = engine.slow_since(&params, 2000).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].request_id, "r3");
    }

    #[test]
    fn slow_since__returns_empty_when_no_new() {
        let engine = seeded_engine();
        let params = super::slow::SlowParams {
            proxy: Some("api".into()),
            threshold_us: 500,
            since_ts: 0,
            tool: None,
            limit: 100,
        };
        let rows = engine.slow_since(&params, 5000).unwrap();
        assert!(rows.is_empty());
    }

    #[test]
    fn slow_since__respects_threshold() {
        let engine = seeded_engine();
        let params = super::slow::SlowParams {
            proxy: Some("api".into()),
            threshold_us: 1000,
            since_ts: 0,
            tool: None,
            limit: 100,
        };
        let rows = engine.slow_since(&params, 0).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].latency_us, 4201);
    }

    #[test]
    fn slow_since__respects_tool_filter() {
        let engine = seeded_engine();
        let params = super::slow::SlowParams {
            proxy: Some("api".into()),
            threshold_us: 500,
            since_ts: 0,
            tool: Some("search".into()),
            limit: 100,
        };
        let rows = engine.slow_since(&params, 0).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].tool.as_deref(), Some("search"));
        assert_eq!(rows[0].latency_us, 891);
    }

    // ── stats ───────────────────────────────────────────────────────────

    #[test]
    fn stats__aggregates_correctly() {
        let engine = seeded_engine();
        let result = engine
            .stats(&super::stats::StatsParams {
                proxy: Some("api".into()),
                since_ts: 0,
            })
            .unwrap();
        assert_eq!(result.total_calls, 5);
        assert!(result.error_pct > 0.0);
        let search = result.tools.iter().find(|t| t.label == "search").unwrap();
        assert_eq!(search.calls, 3);
    }

    #[test]
    fn stats__empty_proxy() {
        let engine = seeded_engine();
        let result = engine
            .stats(&super::stats::StatsParams {
                proxy: Some("nonexistent".into()),
                since_ts: 0,
            })
            .unwrap();
        assert_eq!(result.total_calls, 0);
        assert!(result.tools.is_empty());
    }

    #[test]
    fn stats__latency_us_values() {
        let engine = seeded_engine();
        let result = engine
            .stats(&super::stats::StatsParams {
                proxy: Some("api".into()),
                since_ts: 0,
            })
            .unwrap();

        let search = result.tools.iter().find(|t| t.label == "search").unwrap();
        assert_eq!(search.min_us, 142);
        assert_eq!(search.max_us, 891);
        assert!((search.avg_us - 396.33).abs() < 1.0);
        assert_eq!(search.p95_us, 891);
    }

    #[test]
    fn stats__serialization_uses_us_field_names() {
        let engine = seeded_engine();
        let result = engine
            .stats(&super::stats::StatsParams {
                proxy: Some("api".into()),
                since_ts: 0,
            })
            .unwrap();
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("avg_us"));
        assert!(json.contains("min_us"));
        assert!(json.contains("max_us"));
        assert!(json.contains("p95_us"));
        assert!(!json.contains("avg_ms"));
    }

    #[test]
    fn log_row__latency_us_field() {
        let engine = seeded_engine();
        let rows = engine
            .logs(&super::logs::LogsParams {
                proxy: Some("api".into()),
                since_ts: 0,
                limit: 100,
                tool: Some("search".into()),
                method: None,
                session: None,
                status: None,
                error_code: None,
            })
            .unwrap();
        assert_eq!(rows[0].latency_us, 156);
        assert_eq!(rows[1].latency_us, 891);
        assert_eq!(rows[2].latency_us, 142);
    }

    #[test]
    fn log_row__serialization_uses_us_field() {
        let engine = seeded_engine();
        let rows = engine
            .logs(&super::logs::LogsParams {
                proxy: Some("api".into()),
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
        assert!(json.contains("latency_us"));
        assert!(!json.contains("latency_ms"));
    }

    #[test]
    fn slow__threshold_us_precision() {
        let engine = seeded_engine();
        let rows = engine
            .slow(&super::slow::SlowParams {
                proxy: Some("api".into()),
                tool: None,
                threshold_us: 150,
                since_ts: 0,
                limit: 100,
            })
            .unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].latency_us, 4201);
        assert_eq!(rows[1].latency_us, 891);
        assert_eq!(rows[2].latency_us, 156);
    }

    #[test]
    fn slow__exact_threshold_boundary() {
        let engine = seeded_engine();
        let rows = engine
            .slow(&super::slow::SlowParams {
                proxy: Some("api".into()),
                tool: None,
                threshold_us: 891,
                since_ts: 0,
                limit: 100,
            })
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].latency_us, 4201);
        assert_eq!(rows[1].latency_us, 891);
    }

    // ── clients ─────────────────────────────────────────────────────────

    #[test]
    fn clients__aggregates_by_client() {
        let engine = seeded_engine();
        let rows = engine
            .clients(&super::clients::ClientsParams {
                proxy: Some("api".into()),
                since_ts: 0,
            })
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].client_name.as_deref(), Some("claude-desktop"));
        assert_eq!(rows[0].total_calls, 3);
        assert_eq!(rows[1].client_name.as_deref(), Some("cursor"));
        assert_eq!(rows[1].total_calls, 2);
    }

    // ── sessions ────────────────────────────────────────────────────────

    #[test]
    fn sessions__returns_all() {
        let engine = seeded_engine();
        let rows = engine
            .sessions(&super::sessions::SessionsParams {
                proxy: Some("api".into()),
                since_ts: 0,
                limit: 100,
                active_only: false,
                client: None,
            })
            .unwrap();
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn sessions__filter_by_client() {
        let engine = seeded_engine();
        let rows = engine
            .sessions(&super::sessions::SessionsParams {
                proxy: Some("api".into()),
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
    fn session_detail__returns_with_requests() {
        let engine = seeded_engine();
        let detail = engine.session_detail("s1").unwrap().unwrap();
        assert_eq!(detail.session_id, "s1");
        assert_eq!(detail.client_name.as_deref(), Some("claude-desktop"));
        assert_eq!(detail.client_version.as_deref(), Some("1.2.0"));
        assert_eq!(detail.client_platform.as_deref(), Some("claude"));
        assert_eq!(detail.total_calls, 3);
        assert_eq!(detail.total_errors, 1);
        assert_eq!(detail.requests.len(), 3);
        assert_eq!(detail.requests[0].request_id, "r1");
        assert_eq!(detail.requests[1].request_id, "r2");
        assert_eq!(detail.requests[2].request_id, "r3");
    }

    #[test]
    fn session_detail__closed_session() {
        let engine = seeded_engine();
        let detail = engine.session_detail("s2").unwrap().unwrap();
        assert_eq!(detail.session_id, "s2");
        assert_eq!(detail.client_name.as_deref(), Some("cursor"));
        assert_eq!(detail.ended_at, Some(3500));
        assert_eq!(detail.requests.len(), 2);
        assert_eq!(detail.requests[0].request_id, "r4");
        assert_eq!(detail.requests[1].request_id, "r5");
    }

    #[test]
    fn session_detail__nonexistent_returns_none() {
        let engine = seeded_engine();
        let result = engine.session_detail("no-such-session").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn session_detail__requests_ordered_oldest_first() {
        let engine = seeded_engine();
        let detail = engine.session_detail("s1").unwrap().unwrap();
        for pair in detail.requests.windows(2) {
            assert!(pair[0].ts <= pair[1].ts);
        }
    }

    #[test]
    fn session_detail__serializes_to_json() {
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
    fn vacuum__dry_run_counts_correctly() {
        let engine = seeded_engine();
        let result = engine
            .vacuum(&super::store_ops::VacuumParams {
                before_ts: 3500,
                proxy: None,
                dry_run: true,
            })
            .unwrap();
        assert_eq!(result.deleted_requests, 3);
        assert!(result.dry_run);
    }

    #[test]
    fn vacuum__actually_deletes() {
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

        let remaining = engine
            .logs(&super::logs::LogsParams {
                proxy: Some("api".into()),
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
    fn log_row__serializes_to_json() {
        let engine = seeded_engine();
        let rows = engine
            .logs(&super::logs::LogsParams {
                proxy: Some("api".into()),
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
        assert!(json.contains("latency_us"));
    }

    #[test]
    fn client_row__serializes_to_json() {
        let engine = seeded_engine();
        let rows = engine
            .clients(&super::clients::ClientsParams {
                proxy: Some("api".into()),
                since_ts: 0,
            })
            .unwrap();
        let json = serde_json::to_string(&rows[0]).unwrap();
        assert!(json.contains("client_name"));
        assert!(json.contains("total_calls"));
    }

    #[test]
    fn stats__serializes_to_json() {
        let engine = seeded_engine();
        let result = engine
            .stats(&super::stats::StatsParams {
                proxy: Some("api".into()),
                since_ts: 0,
            })
            .unwrap();
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("total_calls"));
        assert!(json.contains("tools"));
    }

    // ── schema ─────────────────────────────────────────────────────────

    fn seed_schema(engine: &QueryEngine) {
        engine
            .conn()
            .execute(
                "INSERT INTO server_schema (upstream_url, method, payload, captured_at, schema_hash) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    "http://localhost:9000",
                    "initialize",
                    r#"{"serverInfo":{"name":"test-server","version":"1.0"},"protocolVersion":"2025-03-26","capabilities":{"tools":{}}}"#,
                    1000i64,
                    "hash_init"
                ],
            )
            .unwrap();
        engine
            .conn()
            .execute(
                "INSERT INTO server_schema (upstream_url, method, payload, captured_at, schema_hash) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    "http://localhost:9000",
                    "tools/list",
                    r#"{"tools":[{"name":"search","description":"search things"}]}"#,
                    2000i64,
                    "hash_tools"
                ],
            )
            .unwrap();
        engine
            .conn()
            .execute(
                "INSERT INTO schema_changes (upstream_url, method, change_type, item_name, old_hash, new_hash, detected_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params!["http://localhost:9000", "tools/list", "initial", Option::<String>::None, Option::<String>::None, "hash_tools", 2000i64],
            )
            .unwrap();
    }

    /// Insert one `tools/list` schema row + its "initial" change row under
    /// the given proxy name and URL.
    fn seed_schema_for_proxy(engine: &QueryEngine, proxy: &str, upstream: &str) {
        engine
            .conn()
            .execute(
                "INSERT INTO server_schema (proxy, upstream_url, method, payload, captured_at, schema_hash) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    proxy,
                    upstream,
                    "tools/list",
                    r#"{"tools":[{"name":"search","description":"search things"}]}"#,
                    1000i64,
                    format!("hash-{proxy}")
                ],
            )
            .unwrap();
        engine
            .conn()
            .execute(
                "INSERT INTO schema_changes (proxy, upstream_url, method, change_type, item_name, old_hash, new_hash, detected_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    proxy,
                    upstream,
                    "tools/list",
                    "initial",
                    Option::<String>::None,
                    Option::<String>::None,
                    format!("hash-{proxy}"),
                    1000i64,
                ],
            )
            .unwrap();
    }

    #[test]
    fn schema__returns_all_snapshots() {
        let engine = seeded_engine();
        seed_schema(&engine);
        let rows = engine
            .schema(&super::schema::SchemaParams {
                proxy: None,
                method: None,
            })
            .unwrap();
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn schema__filter_by_method() {
        let engine = seeded_engine();
        seed_schema(&engine);
        let rows = engine
            .schema(&super::schema::SchemaParams {
                proxy: None,
                method: Some("tools/list".into()),
            })
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].method, "tools/list");
    }

    #[test]
    fn schema__filter_by_proxy() {
        let engine = seeded_engine();
        seed_schema_for_proxy(&engine, "alpha", "http://a:9000");
        seed_schema_for_proxy(&engine, "beta", "http://b:9000");

        let alpha = engine
            .schema(&super::schema::SchemaParams {
                proxy: Some("alpha".into()),
                method: None,
            })
            .unwrap();
        assert_eq!(alpha.len(), 1);
        assert_eq!(alpha[0].upstream_url, "http://a:9000");

        let beta = engine
            .schema(&super::schema::SchemaParams {
                proxy: Some("beta".into()),
                method: None,
            })
            .unwrap();
        assert_eq!(beta.len(), 1);
        assert_eq!(beta[0].upstream_url, "http://b:9000");

        let missing = engine
            .schema(&super::schema::SchemaParams {
                proxy: Some("nonexistent".into()),
                method: None,
            })
            .unwrap();
        assert!(missing.is_empty());
    }

    #[test]
    fn schema_changes__returns_history() {
        let engine = seeded_engine();
        seed_schema(&engine);
        let rows = engine
            .schema_changes(&super::schema::SchemaChangesParams {
                proxy: None,
                method: None,
                limit: 50,
            })
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].change_type, "initial");
    }

    #[test]
    fn schema_changes__filter_by_proxy() {
        let engine = seeded_engine();
        seed_schema_for_proxy(&engine, "alpha", "http://a:9000");
        seed_schema_for_proxy(&engine, "beta", "http://b:9000");

        let alpha = engine
            .schema_changes(&super::schema::SchemaChangesParams {
                proxy: Some("alpha".into()),
                method: None,
                limit: 50,
            })
            .unwrap();
        assert_eq!(alpha.len(), 1);
        assert_eq!(alpha[0].upstream_url, "http://a:9000");

        let all = engine
            .schema_changes(&super::schema::SchemaChangesParams {
                proxy: None,
                method: None,
                limit: 50,
            })
            .unwrap();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn schema_status__complete() {
        let engine = seeded_engine();
        seed_schema(&engine);
        let status = engine.schema_status("http://localhost:9000").unwrap();
        assert_eq!(status.status, "complete");
        assert_eq!(status.server_name.as_deref(), Some("test-server"));
        assert_eq!(status.server_version.as_deref(), Some("1.0"));
        assert_eq!(status.protocol_version.as_deref(), Some("2025-03-26"));
        assert!(status.capabilities.contains(&"tools".to_string()));
        assert_eq!(status.methods_captured.len(), 2);
    }

    #[test]
    fn schema_status__unknown() {
        let engine = seeded_engine();
        let status = engine.schema_status("http://nonexistent").unwrap();
        assert_eq!(status.status, "unknown");
        assert!(status.methods_captured.is_empty());
    }

    #[test]
    fn schema_status__partial() {
        let engine = seeded_engine();
        engine
            .conn()
            .execute(
                "INSERT INTO server_schema (upstream_url, method, payload, captured_at, schema_hash) VALUES (?1, ?2, ?3, ?4, ?5)",
                params!["http://partial", "tools/list", "{}", 1000i64, "h1"],
            )
            .unwrap();
        let status = engine.schema_status("http://partial").unwrap();
        assert_eq!(status.status, "partial");
    }

    // ── schema unused ──────────────────────────────────────────────────

    #[test]
    fn schema_unused__finds_uncalled_tools() {
        let engine = seeded_engine();
        seed_schema(&engine);

        engine
            .conn()
            .execute(
                "UPDATE server_schema SET payload = ?1 WHERE method = 'tools/list'",
                params![r#"{"tools":[{"name":"search","description":"search things"},{"name":"never_used","description":"does nothing"}]}"#],
            )
            .unwrap();

        let rows = engine
            .schema_unused(&super::schema::SchemaUnusedParams {
                proxy: Some("api".into()),
                since_ts: 0,
            })
            .unwrap();

        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].tool_name, "never_used");
        assert_eq!(rows[0].calls, 0);
        assert_eq!(rows[1].tool_name, "search");
        assert!(rows[1].calls > 0);
    }

    #[test]
    fn schema_unused__empty_when_no_schema() {
        let engine = seeded_engine();
        let rows = engine
            .schema_unused(&super::schema::SchemaUnusedParams {
                proxy: Some("api".into()),
                since_ts: 0,
            })
            .unwrap();
        assert!(rows.is_empty());
    }

    // ── multi-proxy: proxy: None shows all ──────────────────────────────

    /// Seed a second proxy ("email") alongside the default "api" proxy.
    fn seeded_multi_proxy_engine() -> QueryEngine {
        let engine = seeded_engine();

        // Add a second proxy's session and requests.
        engine.conn().execute(
            "INSERT INTO sessions (session_id, proxy, started_at, last_seen_at, client_name, client_version, client_platform, total_calls, total_errors)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params!["s-email-1", "email", 6000, 7000, "claude-desktop", "1.2.0", "claude", 1, 0],
        ).unwrap();

        engine.conn().execute(
            "INSERT INTO requests (request_id, ts, proxy, session_id, method, tool, latency_us, status, error_code, error_msg, bytes_in, bytes_out)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params!["r-email-1", 6000i64, "email", "s-email-1", "tools/call", "send_email", 320i64, "ok", None::<&str>, None::<&str>, Some(512i64), Some(128i64)],
        ).unwrap();

        engine.conn().execute(
            "INSERT INTO requests (request_id, ts, proxy, session_id, method, tool, latency_us, status, error_code, error_msg, bytes_in, bytes_out)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params!["r-email-2", 7000i64, "email", "s-email-1", "tools/call", "send_email", 250i64, "ok", None::<&str>, None::<&str>, Some(512i64), Some(128i64)],
        ).unwrap();

        engine
    }

    #[test]
    fn logs__proxy_none_returns_all() {
        let engine = seeded_multi_proxy_engine();
        let rows = engine
            .logs(&super::logs::LogsParams {
                proxy: None,
                since_ts: 0,
                limit: 100,
                tool: None,
                method: None,
                session: None,
                status: None,
                error_code: None,
            })
            .unwrap();
        assert_eq!(rows.len(), 7);
    }

    #[test]
    fn logs__proxy_filter_excludes_other() {
        let engine = seeded_multi_proxy_engine();
        let rows = engine
            .logs(&super::logs::LogsParams {
                proxy: Some("email".into()),
                since_ts: 0,
                limit: 100,
                tool: None,
                method: None,
                session: None,
                status: None,
                error_code: None,
            })
            .unwrap();
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn stats__proxy_none_aggregates_all() {
        let engine = seeded_multi_proxy_engine();
        let result = engine
            .stats(&super::stats::StatsParams {
                proxy: None,
                since_ts: 0,
            })
            .unwrap();
        assert_eq!(result.total_calls, 7);
    }

    #[test]
    fn stats__proxy_filter_scopes_to_one() {
        let engine = seeded_multi_proxy_engine();
        let result = engine
            .stats(&super::stats::StatsParams {
                proxy: Some("email".into()),
                since_ts: 0,
            })
            .unwrap();
        assert_eq!(result.total_calls, 2);
    }

    #[test]
    fn slow__proxy_none_returns_all() {
        let engine = seeded_multi_proxy_engine();
        let rows = engine
            .slow(&super::slow::SlowParams {
                proxy: None,
                threshold_us: 100,
                since_ts: 0,
                tool: None,
                limit: 100,
            })
            .unwrap();
        assert_eq!(rows.len(), 6);
    }

    #[test]
    fn sessions__proxy_none_returns_all() {
        let engine = seeded_multi_proxy_engine();
        let rows = engine
            .sessions(&super::sessions::SessionsParams {
                proxy: None,
                since_ts: 0,
                limit: 100,
                active_only: false,
                client: None,
            })
            .unwrap();
        assert_eq!(rows.len(), 3);
    }

    #[test]
    fn clients__proxy_none_returns_all() {
        let engine = seeded_multi_proxy_engine();
        let rows = engine
            .clients(&super::clients::ClientsParams {
                proxy: None,
                since_ts: 0,
            })
            .unwrap();
        assert!(rows.len() >= 2);
    }
}
