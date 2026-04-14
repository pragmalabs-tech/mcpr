//! Background storage writer — dedicated OS thread with batch flushing.
//!
//! The writer receives [`StoreEvent`]s through a tokio mpsc channel and writes
//! them to SQLite in batches. It runs on a dedicated OS thread (not a tokio task)
//! because `rusqlite::Connection` is `!Send` — it cannot cross async task boundaries.
//!
//! # Design
//!
//! ```text
//! Proxy hot path (tokio tasks)         Writer thread (OS thread)
//! ─────────────────────────────        ──────────────────────────
//! tx.try_send(event)           ──────► rx.blocking_recv()
//!   (non-blocking, fire-and-forget)    accumulate in batch Vec
//!                                      every 200ms or 500 events:
//!                                        BEGIN TRANSACTION
//!                                        INSERT requests / sessions
//!                                        UPDATE session counters
//!                                        COMMIT
//! ```
//!
//! # Backpressure
//!
//! The channel has a fixed capacity (default 10,000). If the channel is full,
//! `Store::record()` drops the event via `try_send().ok()`. A busy proxy is
//! more important than a complete log.
//!
//! # Shutdown
//!
//! On graceful shutdown, the sender is dropped → `recv()` returns `None` →
//! the writer flushes any remaining batch and exits. This guarantees no
//! events are lost on `mcpr stop` / SIGTERM.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use rusqlite::Connection;
use sha2::{Digest, Sha256};

use mcpr_core::protocol::schema::{self as proto_schema, PageStatus};

use super::event::{RequestStatus, StoreEvent};
use super::schema;

/// How often the writer flushes accumulated events to SQLite.
///
/// 200ms means at most 5 transactions/second even under 1,000 req/s load.
/// SQLite handles this trivially. The 200ms lag is imperceptible in `--follow` mode.
const BATCH_INTERVAL: Duration = Duration::from_millis(200);

/// Maximum events per batch before forcing a flush.
///
/// Caps memory usage of the batch buffer. At ~200 bytes per event,
/// 500 events ≈ 100KB — negligible.
const MAX_BATCH_SIZE: usize = 500;

/// Run the storage writer loop on the current thread (blocking).
///
/// This function blocks forever until the sender is dropped. It is intended
/// to be called from `std::thread::spawn`, not from a tokio task.
///
/// # Arguments
///
/// - `conn`: a read-write SQLite connection (already migrated).
/// - `rx`: the receiving end of the event channel.
pub fn run_writer_loop(conn: Connection, rx: tokio::sync::mpsc::Receiver<StoreEvent>) {
    // We need a blocking receiver. Since tokio mpsc doesn't have a native
    // blocking recv with timeout, we use blocking_recv in a polling loop
    // with try_recv for draining.
    let mut rx = rx;
    let mut batch: Vec<StoreEvent> = Vec::with_capacity(MAX_BATCH_SIZE);
    let mut last_flush = Instant::now();
    // Pagination buffer: (upstream_url, method) → (first_page_ts, accumulated payloads).
    let mut page_buffer: HashMap<(String, String), (Instant, Vec<String>)> = HashMap::new();

    loop {
        // Try to receive one event, blocking up to the remaining batch interval.
        let remaining = BATCH_INTERVAL.saturating_sub(last_flush.elapsed());

        // Use a short poll: block for up to `remaining` duration.
        let event = if remaining.is_zero() {
            // Time to flush — don't wait, just try.
            rx.try_recv().ok()
        } else {
            // Block until an event arrives or timeout expires.
            // We use `blocking_recv` with a manual timeout via try_recv + sleep.
            recv_with_timeout(&mut rx, remaining)
        };

        match event {
            Some(e) => {
                batch.push(e);

                // Drain any additional events already in the channel (non-blocking).
                while batch.len() < MAX_BATCH_SIZE {
                    match rx.try_recv() {
                        Ok(e) => batch.push(e),
                        Err(_) => break,
                    }
                }

                // Flush if batch is full.
                if batch.len() >= MAX_BATCH_SIZE {
                    flush_batch(&conn, &mut batch, &mut page_buffer);
                    last_flush = Instant::now();
                }
            }
            None => {
                // Either timeout (flush interval) or channel closed.
                if !batch.is_empty() {
                    flush_batch(&conn, &mut batch, &mut page_buffer);
                    last_flush = Instant::now();
                }

                // Check if the channel is closed (sender dropped).
                if rx.is_closed() && rx.try_recv().is_err() {
                    // Final drain — sender is gone, no more events coming.
                    break;
                }
            }
        }

        // Time-based flush for partially filled batches.
        if !batch.is_empty() && last_flush.elapsed() >= BATCH_INTERVAL {
            flush_batch(&conn, &mut batch, &mut HashMap::new());
            last_flush = Instant::now();
        }
    }
}

/// Receive one event with a timeout, blocking the current thread.
///
/// tokio's mpsc receiver doesn't have a native `blocking_recv_timeout`,
/// so we poll with short sleeps. The granularity (10ms) is fine — this
/// only affects flush timing, not request latency.
fn recv_with_timeout(
    rx: &mut tokio::sync::mpsc::Receiver<StoreEvent>,
    timeout: Duration,
) -> Option<StoreEvent> {
    let deadline = Instant::now() + timeout;

    loop {
        match rx.try_recv() {
            Ok(event) => return Some(event),
            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => return None,
            Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                if Instant::now() >= deadline {
                    return None;
                }
                // Short sleep to avoid busy-spinning. 10ms granularity is acceptable
                // for a background writer — it only affects batch flush timing.
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

/// Flush all accumulated events to SQLite in a single transaction.
///
/// Session inserts, request inserts, and counter updates all happen in the
/// same transaction — counters are always consistent with the request rows.
fn flush_batch(
    conn: &Connection,
    batch: &mut Vec<StoreEvent>,
    page_buffer: &mut HashMap<(String, String), (Instant, Vec<String>)>,
) {
    if batch.is_empty() {
        return;
    }

    let result = conn.execute_batch("BEGIN TRANSACTION;");
    if let Err(e) = result {
        tracing::warn!("storage writer: failed to begin transaction: {e}");
        batch.clear();
        return;
    }

    for event in batch.drain(..) {
        match event {
            StoreEvent::Session(s) => {
                if let Err(e) = conn.execute(
                    schema::INSERT_SESSION_SQL,
                    rusqlite::params![
                        s.session_id,
                        s.proxy,
                        s.started_at,
                        s.client_name,
                        s.client_version,
                        s.client_platform,
                    ],
                ) {
                    tracing::warn!("storage writer: session insert failed: {e}");
                }
            }

            StoreEvent::Request(r) => {
                if let Err(e) = conn.execute(
                    schema::INSERT_REQUEST_SQL,
                    rusqlite::params![
                        r.request_id,
                        r.ts,
                        r.proxy,
                        r.session_id,
                        r.method,
                        r.tool,
                        r.latency_us,
                        r.status.as_str(),
                        r.error_code,
                        r.error_msg,
                        r.bytes_in,
                        r.bytes_out,
                    ],
                ) {
                    tracing::warn!("storage writer: request insert failed: {e}");
                }

                // Increment session counters in the same transaction.
                if let Some(ref sid) = r.session_id {
                    let error_inc: i64 =
                        if matches!(r.status, RequestStatus::Error | RequestStatus::Timeout) {
                            1
                        } else {
                            0
                        };

                    if let Err(e) = conn.execute(
                        schema::UPDATE_SESSION_COUNTERS_SQL,
                        rusqlite::params![r.ts, error_inc, sid],
                    ) {
                        tracing::warn!("storage writer: session counter update failed: {e}");
                    }
                }
            }

            StoreEvent::SessionClosed {
                session_id,
                ended_at,
            } => {
                if let Err(e) = conn.execute(
                    schema::CLOSE_SESSION_SQL,
                    rusqlite::params![ended_at, session_id],
                ) {
                    tracing::warn!("storage writer: session close failed: {e}");
                }
            }

            StoreEvent::SchemaCapture(sc) => {
                handle_schema_capture(conn, sc, page_buffer);
            }

            StoreEvent::SchemaStale {
                proxy,
                upstream_url,
                method,
                ts,
            } => {
                handle_schema_stale(conn, &proxy, &upstream_url, &method, ts);
            }
        }
    }

    // Expire stale page buffer entries (abandoned pagination, >60s).
    page_buffer.retain(|_, (started, _)| started.elapsed() < Duration::from_secs(60));

    if let Err(e) = conn.execute_batch("COMMIT;") {
        tracing::warn!("storage writer: commit failed: {e}");
    }
}

// ── Schema capture helpers ────────────────────────────────────────────

/// Handle a schema capture event: buffer pages or write immediately.
fn handle_schema_capture(
    conn: &Connection,
    sc: super::event::SchemaCaptureEvent,
    page_buffer: &mut HashMap<(String, String), (Instant, Vec<String>)>,
) {
    let key = (sc.upstream_url.clone(), sc.method.clone());

    match sc.page_status {
        PageStatus::Complete => {
            write_schema(
                conn,
                &sc.proxy,
                &sc.upstream_url,
                &sc.method,
                &sc.payload,
                sc.ts,
            );
        }
        PageStatus::FirstPage => {
            page_buffer.insert(key, (Instant::now(), vec![sc.payload]));
        }
        PageStatus::MiddlePage => {
            if let Some((_, pages)) = page_buffer.get_mut(&key) {
                pages.push(sc.payload);
            }
        }
        PageStatus::LastPage => {
            if let Some((_, mut pages)) = page_buffer.remove(&key) {
                pages.push(sc.payload);
                // Parse accumulated payloads and merge via protocol layer.
                let parsed: Vec<serde_json::Value> = pages
                    .iter()
                    .filter_map(|p| serde_json::from_str(p).ok())
                    .collect();
                if let Some(merged) = proto_schema::merge_pages(&sc.method, &parsed) {
                    let payload = merged.to_string();
                    write_schema(
                        conn,
                        &sc.proxy,
                        &sc.upstream_url,
                        &sc.method,
                        &payload,
                        sc.ts,
                    );
                }
            } else {
                // Missed earlier pages — store what we have (best effort).
                write_schema(
                    conn,
                    &sc.proxy,
                    &sc.upstream_url,
                    &sc.method,
                    &sc.payload,
                    sc.ts,
                );
            }
        }
    }
}

/// Write a schema snapshot to SQLite: hash, diff, upsert.
fn write_schema(
    conn: &Connection,
    proxy: &str,
    upstream_url: &str,
    method: &str,
    payload: &str,
    ts: i64,
) {
    let new_hash = sha256_hex(payload);

    // Check for existing snapshot.
    let existing: Option<(String, String)> = conn
        .query_row(
            schema::GET_SCHEMA_HASH_SQL,
            rusqlite::params![proxy, upstream_url, method],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .ok();

    match existing {
        None => {
            // First capture — insert and record "initial" change.
            if let Err(e) = conn.execute(
                schema::UPSERT_SERVER_SCHEMA_SQL,
                rusqlite::params![proxy, upstream_url, method, payload, ts, new_hash],
            ) {
                tracing::warn!("storage writer: schema upsert failed: {e}");
            }
            if let Err(e) = conn.execute(
                schema::INSERT_SCHEMA_CHANGE_SQL,
                rusqlite::params![
                    proxy,
                    upstream_url,
                    method,
                    "initial",
                    Option::<String>::None,
                    Option::<String>::None,
                    new_hash,
                    ts
                ],
            ) {
                tracing::warn!("storage writer: schema change insert failed: {e}");
            }
        }
        Some((old_hash, _)) if old_hash == new_hash => {
            // Same hash — just refresh the captured_at timestamp.
            if let Err(e) = conn.execute(
                schema::UPSERT_SERVER_SCHEMA_SQL,
                rusqlite::params![proxy, upstream_url, method, payload, ts, new_hash],
            ) {
                tracing::warn!("storage writer: schema upsert failed: {e}");
            }
        }
        Some((old_hash, old_payload)) => {
            // Schema changed — diff and record changes.
            let old_val: serde_json::Value = serde_json::from_str(&old_payload).unwrap_or_default();
            let new_val: serde_json::Value = serde_json::from_str(payload).unwrap_or_default();
            let diffs = proto_schema::diff_schema(method, &old_val, &new_val);

            for diff in &diffs {
                if let Err(e) = conn.execute(
                    schema::INSERT_SCHEMA_CHANGE_SQL,
                    rusqlite::params![
                        proxy,
                        upstream_url,
                        method,
                        diff.change_type,
                        diff.item_name,
                        old_hash,
                        new_hash,
                        ts
                    ],
                ) {
                    tracing::warn!("storage writer: schema change insert failed: {e}");
                }
            }

            // Update the stored schema.
            if let Err(e) = conn.execute(
                schema::UPSERT_SERVER_SCHEMA_SQL,
                rusqlite::params![proxy, upstream_url, method, payload, ts, new_hash],
            ) {
                tracing::warn!("storage writer: schema upsert failed: {e}");
            }
        }
    }
}

/// Record a stale marker in schema_changes.
fn handle_schema_stale(conn: &Connection, proxy: &str, upstream_url: &str, method: &str, ts: i64) {
    let current_hash: Option<String> = conn
        .query_row(
            "SELECT schema_hash FROM server_schema WHERE proxy = ?1 AND upstream_url = ?2 AND method = ?3",
            rusqlite::params![proxy, upstream_url, method],
            |row| row.get(0),
        )
        .ok();

    if let Err(e) = conn.execute(
        schema::INSERT_SCHEMA_CHANGE_SQL,
        rusqlite::params![
            proxy,
            upstream_url,
            method,
            "stale",
            Option::<String>::None,
            current_hash,
            Option::<String>::None,
            ts
        ],
    ) {
        tracing::warn!("storage writer: schema stale insert failed: {e}");
    }
}

/// Compute SHA-256 hex hash of a string.
fn sha256_hex(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::db;
    use crate::store::event::{RequestEvent, SessionEvent};

    /// Helper: create an in-memory DB with schema applied.
    fn test_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA journal_mode = WAL;").ok(); // WAL may not work in-memory, that's fine
        db::run_migrations(&conn, "test").unwrap();
        conn
    }

    #[test]
    fn flush_batch_inserts_session_and_request() {
        let conn = test_db();
        let mut batch = vec![
            StoreEvent::Session(SessionEvent {
                session_id: "sess-1".into(),
                proxy: "api".into(),
                started_at: 1000,
                client_name: Some("claude-desktop".into()),
                client_version: Some("1.0.0".into()),
                client_platform: Some("claude".into()),
            }),
            StoreEvent::Request(RequestEvent {
                request_id: "req-1".into(),
                ts: 1001,
                proxy: "api".into(),
                session_id: Some("sess-1".into()),
                method: "tools/call".into(),
                tool: Some("search".into()),
                latency_us: 142,
                status: RequestStatus::Ok,
                error_code: None,
                error_msg: None,
                bytes_in: Some(256),
                bytes_out: Some(1024),
            }),
        ];

        flush_batch(&conn, &mut batch, &mut HashMap::new());

        // Verify session was inserted
        let client: String = conn
            .query_row(
                "SELECT client_name FROM sessions WHERE session_id = 'sess-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(client, "claude-desktop");

        // Verify request was inserted
        let tool: String = conn
            .query_row(
                "SELECT tool FROM requests WHERE request_id = 'req-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(tool, "search");

        // Verify session counters were updated
        let (calls, errors): (i64, i64) = conn
            .query_row(
                "SELECT total_calls, total_errors FROM sessions WHERE session_id = 'sess-1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(calls, 1);
        assert_eq!(errors, 0);

        // Verify latency_us is stored correctly
        let latency: i64 = conn
            .query_row(
                "SELECT latency_us FROM requests WHERE request_id = 'req-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(latency, 142, "latency_us should be stored as-is in μs");
    }

    #[test]
    fn flush_batch_sub_ms_latency() {
        let conn = test_db();
        let mut batch = vec![
            StoreEvent::Session(SessionEvent {
                session_id: "sess-sub".into(),
                proxy: "api".into(),
                started_at: 1000,
                client_name: None,
                client_version: None,
                client_platform: None,
            }),
            StoreEvent::Request(RequestEvent {
                request_id: "req-fast".into(),
                ts: 1001,
                proxy: "api".into(),
                session_id: Some("sess-sub".into()),
                method: "tools/call".into(),
                tool: Some("ping".into()),
                latency_us: 200, // 200μs — sub-millisecond
                status: RequestStatus::Ok,
                error_code: None,
                error_msg: None,
                bytes_in: Some(64),
                bytes_out: Some(32),
            }),
        ];

        flush_batch(&conn, &mut batch, &mut HashMap::new());

        let latency: i64 = conn
            .query_row(
                "SELECT latency_us FROM requests WHERE request_id = 'req-fast'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(latency, 200, "sub-millisecond latency should be preserved");
    }

    #[test]
    fn flush_batch_session_closed() {
        let conn = test_db();

        // Insert a session first.
        let mut batch = vec![StoreEvent::Session(SessionEvent {
            session_id: "sess-2".into(),
            proxy: "api".into(),
            started_at: 2000,
            client_name: None,
            client_version: None,
            client_platform: None,
        })];
        flush_batch(&conn, &mut batch, &mut HashMap::new());

        // Close it.
        let mut batch = vec![StoreEvent::SessionClosed {
            session_id: "sess-2".into(),
            ended_at: 3000,
        }];
        flush_batch(&conn, &mut batch, &mut HashMap::new());

        let ended: i64 = conn
            .query_row(
                "SELECT ended_at FROM sessions WHERE session_id = 'sess-2'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(ended, 3000);
    }

    #[test]
    fn flush_batch_error_increments_counter() {
        let conn = test_db();

        let mut batch = vec![
            StoreEvent::Session(SessionEvent {
                session_id: "sess-3".into(),
                proxy: "api".into(),
                started_at: 3000,
                client_name: None,
                client_version: None,
                client_platform: None,
            }),
            StoreEvent::Request(RequestEvent {
                request_id: "req-err-1".into(),
                ts: 3001,
                proxy: "api".into(),
                session_id: Some("sess-3".into()),
                method: "tools/call".into(),
                tool: Some("broken".into()),
                latency_us: 500,
                status: RequestStatus::Error,
                error_code: Some("-32600".into()),
                error_msg: Some("bad request".into()),
                bytes_in: None,
                bytes_out: None,
            }),
        ];
        flush_batch(&conn, &mut batch, &mut HashMap::new());

        let (calls, errors): (i64, i64) = conn
            .query_row(
                "SELECT total_calls, total_errors FROM sessions WHERE session_id = 'sess-3'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(calls, 1);
        assert_eq!(errors, 1);
    }

    // ── Schema capture tests ─────────────────────────────────────────

    use crate::store::event::SchemaCaptureEvent as StoreSchemaCapture;

    fn tools_payload(names: &[&str]) -> String {
        let tools: Vec<serde_json::Value> = names
            .iter()
            .map(|n| serde_json::json!({"name": n, "description": format!("tool {n}")}))
            .collect();
        serde_json::json!({"tools": tools}).to_string()
    }

    #[test]
    fn flush_batch_inserts_schema_initial() {
        let conn = test_db();
        let payload = tools_payload(&["search", "create"]);
        let mut batch = vec![StoreEvent::SchemaCapture(StoreSchemaCapture {
            ts: 1000,
            proxy: "api".into(),
            upstream_url: "http://localhost:9000".into(),
            method: "tools/list".into(),
            payload: payload.clone(),
            page_status: PageStatus::Complete,
        })];
        flush_batch(&conn, &mut batch, &mut HashMap::new());

        // Verify server_schema row.
        let (method, hash): (String, String) = conn
            .query_row(
                "SELECT method, schema_hash FROM server_schema WHERE upstream_url = 'http://localhost:9000'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(method, "tools/list");
        assert!(!hash.is_empty());

        // Verify "initial" change.
        let change_type: String = conn
            .query_row(
                "SELECT change_type FROM schema_changes WHERE upstream_url = 'http://localhost:9000'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(change_type, "initial");
    }

    #[test]
    fn flush_batch_schema_unchanged_no_new_change() {
        let conn = test_db();
        let payload = tools_payload(&["search"]);

        // First capture.
        let mut batch = vec![StoreEvent::SchemaCapture(StoreSchemaCapture {
            ts: 1000,
            proxy: "api".into(),
            upstream_url: "http://localhost:9000".into(),
            method: "tools/list".into(),
            payload: payload.clone(),
            page_status: PageStatus::Complete,
        })];
        flush_batch(&conn, &mut batch, &mut HashMap::new());

        // Same payload again.
        let mut batch = vec![StoreEvent::SchemaCapture(StoreSchemaCapture {
            ts: 2000,
            proxy: "api".into(),
            upstream_url: "http://localhost:9000".into(),
            method: "tools/list".into(),
            payload,
            page_status: PageStatus::Complete,
        })];
        flush_batch(&conn, &mut batch, &mut HashMap::new());

        // Only 1 change record (the initial one).
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM schema_changes", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);

        // But captured_at was updated.
        let captured_at: i64 = conn
            .query_row(
                "SELECT captured_at FROM server_schema WHERE upstream_url = 'http://localhost:9000'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(captured_at, 2000);
    }

    #[test]
    fn flush_batch_schema_diff_records_changes() {
        let conn = test_db();

        // Initial: tools a, b.
        let mut batch = vec![StoreEvent::SchemaCapture(StoreSchemaCapture {
            ts: 1000,
            proxy: "api".into(),
            upstream_url: "http://localhost:9000".into(),
            method: "tools/list".into(),
            payload: tools_payload(&["a", "b"]),
            page_status: PageStatus::Complete,
        })];
        flush_batch(&conn, &mut batch, &mut HashMap::new());

        // Changed: tools a, c (b removed, c added).
        let mut batch = vec![StoreEvent::SchemaCapture(StoreSchemaCapture {
            ts: 2000,
            proxy: "api".into(),
            upstream_url: "http://localhost:9000".into(),
            method: "tools/list".into(),
            payload: tools_payload(&["a", "c"]),
            page_status: PageStatus::Complete,
        })];
        flush_batch(&conn, &mut batch, &mut HashMap::new());

        // Should have: initial + tool_removed(b) + tool_added(c).
        let mut stmt = conn
            .prepare("SELECT change_type, item_name FROM schema_changes ORDER BY id")
            .unwrap();
        let changes: Vec<(String, Option<String>)> = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();

        assert_eq!(changes[0].0, "initial");
        let change_types: Vec<&str> = changes[1..].iter().map(|(t, _)| t.as_str()).collect();
        assert!(change_types.contains(&"tool_added"));
        assert!(change_types.contains(&"tool_removed"));
    }

    #[test]
    fn flush_batch_schema_stale() {
        let conn = test_db();

        // Insert initial schema first.
        let mut batch = vec![StoreEvent::SchemaCapture(StoreSchemaCapture {
            ts: 1000,
            proxy: "api".into(),
            upstream_url: "http://localhost:9000".into(),
            method: "tools/list".into(),
            payload: tools_payload(&["search"]),
            page_status: PageStatus::Complete,
        })];
        flush_batch(&conn, &mut batch, &mut HashMap::new());

        // Mark as stale.
        let mut batch = vec![StoreEvent::SchemaStale {
            proxy: "api".into(),
            upstream_url: "http://localhost:9000".into(),
            method: "tools/list".into(),
            ts: 2000,
        }];
        flush_batch(&conn, &mut batch, &mut HashMap::new());

        // Should have "initial" + "stale" changes.
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM schema_changes", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 2);

        let stale_type: String = conn
            .query_row(
                "SELECT change_type FROM schema_changes ORDER BY id DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stale_type, "stale");
    }

    #[test]
    fn flush_batch_pagination_merges() {
        let conn = test_db();
        let mut page_buffer = HashMap::new();

        // First page.
        let mut batch = vec![StoreEvent::SchemaCapture(StoreSchemaCapture {
            ts: 1000,
            proxy: "api".into(),
            upstream_url: "http://localhost:9000".into(),
            method: "tools/list".into(),
            payload: r#"{"tools":[{"name":"a","description":"tool a"}]}"#.into(),
            page_status: PageStatus::FirstPage,
        })];
        flush_batch(&conn, &mut batch, &mut page_buffer);

        // Not written yet — still buffering.
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM server_schema", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);

        // Last page.
        let mut batch = vec![StoreEvent::SchemaCapture(StoreSchemaCapture {
            ts: 2000,
            proxy: "api".into(),
            upstream_url: "http://localhost:9000".into(),
            method: "tools/list".into(),
            payload: r#"{"tools":[{"name":"b","description":"tool b"}]}"#.into(),
            page_status: PageStatus::LastPage,
        })];
        flush_batch(&conn, &mut batch, &mut page_buffer);

        // Now written — merged payload should have both tools.
        let payload: String = conn
            .query_row(
                "SELECT payload FROM server_schema WHERE upstream_url = 'http://localhost:9000'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let val: serde_json::Value = serde_json::from_str(&payload).unwrap();
        let tools = val["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 2);
    }

    #[test]
    fn sha256_hex_deterministic() {
        let h1 = sha256_hex("hello");
        let h2 = sha256_hex("hello");
        assert_eq!(h1, h2);
        assert_ne!(sha256_hex("hello"), sha256_hex("world"));
        assert_eq!(h1.len(), 64); // SHA-256 hex is 64 chars
    }
}
