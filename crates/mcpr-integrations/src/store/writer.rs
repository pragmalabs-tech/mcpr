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
//! events are lost on SIGTERM.

use std::time::{Duration, Instant};

use rusqlite::Connection;

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
    let mut rx = rx;
    let mut batch: Vec<StoreEvent> = Vec::with_capacity(MAX_BATCH_SIZE);
    let mut last_flush = Instant::now();

    loop {
        let remaining = BATCH_INTERVAL.saturating_sub(last_flush.elapsed());

        let event = if remaining.is_zero() {
            rx.try_recv().ok()
        } else {
            recv_with_timeout(&mut rx, remaining)
        };

        match event {
            Some(e) => {
                batch.push(e);

                while batch.len() < MAX_BATCH_SIZE {
                    match rx.try_recv() {
                        Ok(e) => batch.push(e),
                        Err(_) => break,
                    }
                }

                if batch.len() >= MAX_BATCH_SIZE {
                    flush_batch(&conn, &mut batch);
                    last_flush = Instant::now();
                }
            }
            None => {
                if !batch.is_empty() {
                    flush_batch(&conn, &mut batch);
                    last_flush = Instant::now();
                }

                if rx.is_closed() && rx.try_recv().is_err() {
                    break;
                }
            }
        }

        if !batch.is_empty() && last_flush.elapsed() >= BATCH_INTERVAL {
            flush_batch(&conn, &mut batch);
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
fn flush_batch(conn: &Connection, batch: &mut Vec<StoreEvent>) {
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
                        r.resource_uri,
                        r.prompt_name,
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

            StoreEvent::SchemaVersion(sv) => {
                handle_schema_version(conn, sv);
            }
        }
    }

    if let Err(e) = conn.execute_batch("COMMIT;") {
        tracing::warn!("storage writer: commit failed: {e}");
    }
}

// ── Schema version persistence ────────────────────────────────────────

/// Persist a new `SchemaVersion` event: append change rows, upsert snapshot.
///
/// The event only fires on content change (SchemaManager guarantees this),
/// so we don't re-hash or re-check for equality. We only read the prior
/// payload to compute granular diff rows for `schema_changes`.
fn handle_schema_version(conn: &Connection, sv: super::event::SchemaVersionEvent) {
    let prior: Option<(String, String)> = conn
        .query_row(
            schema::GET_SCHEMA_HASH_SQL,
            rusqlite::params![sv.proxy, sv.upstream_url, sv.method],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .ok();

    match prior {
        None => {
            insert_change_row(
                conn,
                &sv,
                "initial",
                None,
                None,
                Some(sv.content_hash.as_str()),
            );
        }
        Some((old_hash, old_payload)) => {
            // TODO: Replace by new method
        }
    }

    if let Err(e) = conn.execute(
        schema::UPSERT_SERVER_SCHEMA_SQL,
        rusqlite::params![
            sv.proxy,
            sv.upstream_url,
            sv.method,
            sv.payload,
            sv.ts,
            sv.content_hash
        ],
    ) {
        tracing::warn!("storage writer: schema upsert failed: {e}");
    }
}

fn insert_change_row(
    conn: &Connection,
    sv: &super::event::SchemaVersionEvent,
    change_type: &str,
    item_name: Option<&str>,
    old_hash: Option<&str>,
    new_hash: Option<&str>,
) {
    if let Err(e) = conn.execute(
        schema::INSERT_SCHEMA_CHANGE_SQL,
        rusqlite::params![
            sv.proxy,
            sv.upstream_url,
            sv.method,
            change_type,
            item_name,
            old_hash,
            new_hash,
            sv.ts
        ],
    ) {
        tracing::warn!("storage writer: schema change insert failed: {e}");
    }
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;
    use crate::store::db;
    use crate::store::event::{RequestEvent, SchemaVersionEvent, SessionEvent};

    fn test_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA journal_mode = WAL;").ok();
        db::run_migrations(&conn, "test").unwrap();
        conn
    }

    fn session_event(id: &str) -> SessionEvent {
        SessionEvent {
            session_id: id.into(),
            proxy: "api".into(),
            started_at: 1000,
            client_name: Some("claude-desktop".into()),
            client_version: Some("1.0.0".into()),
            client_platform: Some("claude".into()),
        }
    }

    fn request_event(id: &str, session_id: &str, status: RequestStatus) -> RequestEvent {
        RequestEvent {
            request_id: id.into(),
            ts: 1001,
            proxy: "api".into(),
            session_id: Some(session_id.into()),
            method: "tools/call".into(),
            tool: Some("search".into()),
            resource_uri: None,
            prompt_name: None,
            latency_us: 142,
            status,
            error_code: None,
            error_msg: None,
            bytes_in: Some(256),
            bytes_out: Some(1024),
        }
    }

    fn tools_payload(names: &[&str]) -> String {
        let tools: Vec<serde_json::Value> = names
            .iter()
            .map(|n| serde_json::json!({"name": n, "description": format!("tool {n}")}))
            .collect();
        serde_json::json!({"tools": tools}).to_string()
    }

    fn schema_version_event(payload: &str, hash: &str, ts: i64) -> SchemaVersionEvent {
        SchemaVersionEvent {
            ts,
            proxy: "api".into(),
            upstream_url: "http://localhost:9000".into(),
            method: "tools/list".into(),
            payload: payload.to_string(),
            content_hash: hash.to_string(),
        }
    }

    #[test]
    fn flush_batch__inserts_session_and_request() {
        let conn = test_db();
        let mut batch = vec![
            StoreEvent::Session(session_event("sess-1")),
            StoreEvent::Request(request_event("req-1", "sess-1", RequestStatus::Ok)),
        ];
        flush_batch(&conn, &mut batch);

        let client: String = conn
            .query_row(
                "SELECT client_name FROM sessions WHERE session_id = 'sess-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(client, "claude-desktop");

        let (calls, errors): (i64, i64) = conn
            .query_row(
                "SELECT total_calls, total_errors FROM sessions WHERE session_id = 'sess-1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(calls, 1);
        assert_eq!(errors, 0);
    }

    #[test]
    fn flush_batch__session_closed() {
        let conn = test_db();
        let mut batch = vec![StoreEvent::Session(session_event("sess-2"))];
        flush_batch(&conn, &mut batch);

        let mut batch = vec![StoreEvent::SessionClosed {
            session_id: "sess-2".into(),
            ended_at: 3000,
        }];
        flush_batch(&conn, &mut batch);

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
    fn flush_batch__error_increments_counter() {
        let conn = test_db();
        let mut batch = vec![
            StoreEvent::Session(session_event("sess-3")),
            StoreEvent::Request(request_event("req-err-1", "sess-3", RequestStatus::Error)),
        ];
        flush_batch(&conn, &mut batch);

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

    // ── SchemaVersion persistence ────────────────────────────────────

    #[test]
    fn schema_version__first_ingest_records_initial() {
        let conn = test_db();
        let payload = tools_payload(&["search", "create"]);
        let mut batch = vec![StoreEvent::SchemaVersion(schema_version_event(
            &payload, "hash-v1", 1000,
        ))];
        flush_batch(&conn, &mut batch);

        let (method, hash): (String, String) = conn
            .query_row(
                "SELECT method, schema_hash FROM server_schema WHERE upstream_url = 'http://localhost:9000'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(method, "tools/list");
        assert_eq!(hash, "hash-v1");

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
    fn schema_version__second_ingest_records_granular_diff() {
        let conn = test_db();

        let mut batch = vec![StoreEvent::SchemaVersion(schema_version_event(
            &tools_payload(&["a", "b"]),
            "hash-v1",
            1000,
        ))];
        flush_batch(&conn, &mut batch);

        let mut batch = vec![StoreEvent::SchemaVersion(schema_version_event(
            &tools_payload(&["a", "c"]),
            "hash-v2",
            2000,
        ))];
        flush_batch(&conn, &mut batch);

        let mut stmt = conn
            .prepare("SELECT change_type, item_name FROM schema_changes ORDER BY id")
            .unwrap();
        let changes: Vec<(String, Option<String>)> = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();

        assert_eq!(changes[0].0, "initial");
        let later_types: Vec<&str> = changes[1..].iter().map(|(t, _)| t.as_str()).collect();
        assert!(later_types.contains(&"tool_added"));
        assert!(later_types.contains(&"tool_removed"));
    }

    #[test]
    fn schema_version__upsert_overwrites_payload_and_hash() {
        let conn = test_db();

        let mut batch = vec![StoreEvent::SchemaVersion(schema_version_event(
            &tools_payload(&["a"]),
            "hash-v1",
            1000,
        ))];
        flush_batch(&conn, &mut batch);

        let mut batch = vec![StoreEvent::SchemaVersion(schema_version_event(
            &tools_payload(&["a", "b"]),
            "hash-v2",
            2000,
        ))];
        flush_batch(&conn, &mut batch);

        let (hash, captured_at): (String, i64) = conn
            .query_row(
                "SELECT schema_hash, captured_at FROM server_schema WHERE upstream_url = 'http://localhost:9000'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(hash, "hash-v2");
        assert_eq!(captured_at, 2000);
    }
}
