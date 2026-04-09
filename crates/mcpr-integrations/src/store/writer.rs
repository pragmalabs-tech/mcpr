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
    // We need a blocking receiver. Since tokio mpsc doesn't have a native
    // blocking recv with timeout, we use blocking_recv in a polling loop
    // with try_recv for draining.
    let mut rx = rx;
    let mut batch: Vec<StoreEvent> = Vec::with_capacity(MAX_BATCH_SIZE);
    let mut last_flush = Instant::now();

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
                    flush_batch(&conn, &mut batch);
                    last_flush = Instant::now();
                }
            }
            None => {
                // Either timeout (flush interval) or channel closed.
                if !batch.is_empty() {
                    flush_batch(&conn, &mut batch);
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
                        r.latency_ms,
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
        }
    }

    if let Err(e) = conn.execute_batch("COMMIT;") {
        tracing::warn!("storage writer: commit failed: {e}");
    }
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
                latency_ms: 142,
                status: RequestStatus::Ok,
                error_code: None,
                error_msg: None,
                bytes_in: Some(256),
                bytes_out: Some(1024),
            }),
        ];

        flush_batch(&conn, &mut batch);

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
        flush_batch(&conn, &mut batch);

        // Close it.
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
                latency_ms: 500,
                status: RequestStatus::Error,
                error_code: Some("-32600".into()),
                error_msg: Some("bad request".into()),
                bytes_in: None,
                bytes_out: None,
            }),
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
}
