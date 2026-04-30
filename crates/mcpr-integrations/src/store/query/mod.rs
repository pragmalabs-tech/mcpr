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
    ///
    /// Runs migrations on open. The writer also migrates on `Store::open`,
    /// but a user who upgrades the binary and runs a read command before
    /// restarting the proxy would otherwise hit "no such column" errors
    /// against a stale schema. Migrations are idempotent + WAL-safe, so
    /// it's fine for the reader to bump the schema if the writer hasn't
    /// caught up yet.
    pub fn open(db_path: &Path) -> Result<Self, rusqlite::Error> {
        let conn = db::open_connection(db_path)?;
        db::init_schema(&conn, env!("CARGO_PKG_VERSION"))?;
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

    /// Build a test QueryEngine with the new schema and a fixed seed of
    /// sessions, requests, and responses. Latencies are encoded as
    /// `(response_ts - request_ts)` in milliseconds; the `request_log`
    /// view multiplies by 1000 to expose `latency_us`. Test assertions
    /// reference the resulting `latency_us` values.
    pub(crate) fn seeded_engine() -> QueryEngine {
        let conn = Connection::open_in_memory().unwrap();
        db::init_schema(&conn, "test").unwrap();

        // Sessions: s1 active (claude-desktop), s2 closed (cursor).
        // `state = 'closed'` plus `last_active = 3500` makes the view's
        // `ended_at` resolve to 3500.
        conn.execute(
            "INSERT INTO sessions (id, proxy, state, client_name, client_version,
                                   created_at, last_active, request_count)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                "s1",
                "api",
                "active",
                "claude-desktop",
                "1.2.0",
                1000,
                5000,
                3
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions (id, proxy, state, client_name, client_version,
                                   created_at, last_active, request_count)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params!["s2", "api", "closed", "cursor", "0.44", 2000, 3500, 2],
        )
        .unwrap();

        // Requests + paired responses. Order matches the legacy fixture
        // so existing test assertions over `request_id` keep working.
        // (req.ts, resp.ts, latency_ms_diff, latency_us_for_assertions):
        //   r1: 1000, 1142,   142 →   142_000
        //   r2: 2000, 2891,   891 →   891_000
        //   r3: 3000, 7201,  4201 → 4_201_000   (error)
        //   r4: 4000, 4023,    23 →    23_000
        //   r5: 5000, 5156,   156 →   156_000
        let requests: &[(&str, i64, Option<&str>, &str, Option<&str>, Option<i64>)] = &[
            (
                "r1",
                1000,
                Some("s1"),
                "tools/call",
                Some("search"),
                Some(256),
            ),
            (
                "r2",
                2000,
                Some("s1"),
                "tools/call",
                Some("search"),
                Some(256),
            ),
            (
                "r3",
                3000,
                Some("s1"),
                "tools/call",
                Some("create_order"),
                Some(512),
            ),
            ("r4", 4000, Some("s2"), "resources/read", None, Some(64)),
            (
                "r5",
                5000,
                Some("s2"),
                "tools/call",
                Some("search"),
                Some(256),
            ),
        ];
        for (rid, ts, sid, method, tool, bytes_in) in requests {
            conn.execute(
                "INSERT INTO requests (ts, proxy, session_id, request_id, method, tool, bytes_in)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![ts, "api", sid, rid, method, tool, bytes_in],
            )
            .unwrap();
        }

        let responses: &[(
            &str,
            i64,
            Option<&str>,
            &str,
            Option<i64>,
            Option<&str>,
            Option<i64>,
        )] = &[
            ("r1", 1142, Some("s1"), "ok", None, None, Some(1024)),
            ("r2", 2891, Some("s1"), "ok", None, None, Some(4096)),
            (
                "r3",
                7201,
                Some("s1"),
                "error",
                Some(-32600),
                Some("timeout"),
                None,
            ),
            ("r4", 4023, Some("s2"), "ok", None, None, Some(2048)),
            ("r5", 5156, Some("s2"), "ok", None, None, Some(1024)),
        ];
        for (rid, ts, sid, status, code, msg, bytes_out) in responses {
            conn.execute(
                "INSERT INTO responses (ts, session_id, request_id, status, error_code, error_msg, bytes_out)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![ts, sid, rid, status, code, msg, bytes_out],
            )
            .unwrap();
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
                threshold_us: 500_000,
                since_ts: 0,
                limit: 100,
            })
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].tool.as_deref(), Some("search"));
        assert_eq!(rows[0].latency_us, 891_000);
    }

    #[test]
    fn slow__returns_above_threshold() {
        let engine = seeded_engine();
        let rows = engine
            .slow(&super::slow::SlowParams {
                proxy: Some("api".into()),
                tool: None,
                threshold_us: 500_000,
                since_ts: 0,
                limit: 100,
            })
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].latency_us, 4_201_000);
        assert_eq!(rows[1].latency_us, 891_000);
    }

    #[test]
    fn slow__high_threshold_returns_empty() {
        let engine = seeded_engine();
        let rows = engine
            .slow(&super::slow::SlowParams {
                proxy: Some("api".into()),
                tool: None,
                threshold_us: 10_000_000,
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
            threshold_us: 500_000,
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
            threshold_us: 500_000,
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
            threshold_us: 500_000,
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
            threshold_us: 1_000_000,
            since_ts: 0,
            tool: None,
            limit: 100,
        };
        let rows = engine.slow_since(&params, 0).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].latency_us, 4_201_000);
    }

    #[test]
    fn slow_since__respects_tool_filter() {
        let engine = seeded_engine();
        let params = super::slow::SlowParams {
            proxy: Some("api".into()),
            threshold_us: 500_000,
            since_ts: 0,
            tool: Some("search".into()),
            limit: 100,
        };
        let rows = engine.slow_since(&params, 0).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].tool.as_deref(), Some("search"));
        assert_eq!(rows[0].latency_us, 891_000);
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
        assert_eq!(search.min_us, 142_000);
        assert_eq!(search.max_us, 891_000);
        assert!((search.avg_us - 396_333.33).abs() < 1.0);
        assert_eq!(search.p95_us, 891_000);
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
        assert_eq!(rows[0].latency_us, 156_000);
        assert_eq!(rows[1].latency_us, 891_000);
        assert_eq!(rows[2].latency_us, 142_000);
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
                threshold_us: 150_000,
                since_ts: 0,
                limit: 100,
            })
            .unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].latency_us, 4_201_000);
        assert_eq!(rows[1].latency_us, 891_000);
        assert_eq!(rows[2].latency_us, 156_000);
    }

    #[test]
    fn slow__exact_threshold_boundary() {
        let engine = seeded_engine();
        let rows = engine
            .slow(&super::slow::SlowParams {
                proxy: Some("api".into()),
                tool: None,
                threshold_us: 891_000,
                since_ts: 0,
                limit: 100,
            })
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].latency_us, 4_201_000);
        assert_eq!(rows[1].latency_us, 891_000);
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
    //
    // The legacy `server_schema` test suite asserted behavior that the
    // new event model can't express (per-method payloads, upstream_url
    // tracking, initialize-derived serverInfo). Those tests were dropped
    // along with the table. The remaining `schema_status__unknown` test
    // verifies the new "always unknown" behavior of the stub impl.

    #[test]
    fn schema_status__unknown() {
        let engine = seeded_engine();
        let status = engine.schema_status("http://nonexistent").unwrap();
        assert_eq!(status.status, "unknown");
        assert!(status.methods_captured.is_empty());
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

    #[test]
    fn latest_schema_row__none_when_missing() {
        // The new model has no server_schema table; the function always
        // returns None until a per-kind hydrator is built.
        let engine = seeded_engine();
        assert!(
            engine
                .latest_schema_row("nonexistent", "tools/list")
                .unwrap()
                .is_none()
        );
    }

    // ── multi-proxy: proxy: None shows all ──────────────────────────────

    /// Seed a second proxy ("email") alongside the default "api" proxy.
    fn seeded_multi_proxy_engine() -> QueryEngine {
        let engine = seeded_engine();

        engine
            .conn()
            .execute(
                "INSERT INTO sessions (id, proxy, state, client_name, client_version,
                                       created_at, last_active, request_count)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    "s-email-1",
                    "email",
                    "active",
                    "claude-desktop",
                    "1.2.0",
                    6000,
                    7000,
                    2,
                ],
            )
            .unwrap();

        let email_requests: &[(&str, i64, &str)] = &[
            ("r-email-1", 6000, "send_email"),
            ("r-email-2", 7000, "send_email"),
        ];
        for (rid, ts, tool) in email_requests {
            engine
                .conn()
                .execute(
                    "INSERT INTO requests (ts, proxy, session_id, request_id, method, tool, bytes_in)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![ts, "email", "s-email-1", rid, "tools/call", tool, 512i64],
                )
                .unwrap();
        }

        // Both email responses arrive 200ms after their requests, giving
        // latency_us = 200_000 — comfortably above the slow-test threshold
        // so the multi-proxy "show all" cases see them.
        let email_responses: &[(&str, i64)] = &[("r-email-1", 6200), ("r-email-2", 7200)];
        for (rid, ts) in email_responses {
            engine
                .conn()
                .execute(
                    "INSERT INTO responses (ts, session_id, request_id, status, bytes_out)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![ts, "s-email-1", rid, "ok", 128i64],
                )
                .unwrap();
        }

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
                threshold_us: 100_000,
                since_ts: 0,
                tool: None,
                limit: 100,
            })
            .unwrap();
        // 4 of the 5 seeded main-proxy requests have latency_us >= 100_000
        // (only r4 = 23_000 falls below). Both email rows are at 200_000.
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
