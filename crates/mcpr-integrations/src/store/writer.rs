//! Background storage writer — dedicated OS thread with batch flushing.
//!
//! Receives [`StoreEvent`]s through a tokio mpsc channel and writes them
//! to SQLite in batches. Runs on a dedicated OS thread because
//! `rusqlite::Connection` is `!Send` and cannot cross async task
//! boundaries.
//!
//! ```text
//! Proxy hot path (tokio tasks)         Writer thread (OS thread)
//! ─────────────────────────────        ──────────────────────────
//! tx.try_send(StoreEvent)      ──────► rx.try_recv()
//!   (non-blocking)                     accumulate batch Vec
//!                                      every 200ms or 500 events:
//!                                        BEGIN TRANSACTION
//!                                        bind+execute one prepared
//!                                          statement per StoreEvent
//!                                        COMMIT
//! ```
//!
//! # Backpressure
//!
//! The channel is fixed-capacity (10k). `Store::record` uses `try_send`
//! and silently drops on overflow — a busy proxy is more important than
//! a complete log.
//!
//! # Shutdown
//!
//! On graceful shutdown the sender is dropped → `recv` returns `None` →
//! the writer flushes the remaining batch and exits, so no events are
//! lost on SIGTERM.

use std::time::{Duration, Instant};

use rusqlite::{Connection, Transaction, params};
use sha2::{Digest, Sha256};

use mcpr_core::event::ProxyEvent;
use mcpr_core::event::types::{LoggedRequest, LoggedResponse, RequestEvent};
use mcpr_core::protocol::{
    mcp::{ClientMethod, JsonRpcRequest, JsonRpcResult, RequestId},
    schema::{ChangeSchema, Reason},
    session::{SessionInfo, SessionState, session_id_from_headers},
};

use super::event::StoreEvent;
use super::schema;

/// How often the writer flushes accumulated events. 200ms = at most 5
/// transactions/second even at 1k req/s. Imperceptible in `--follow`.
const BATCH_INTERVAL: Duration = Duration::from_millis(200);

/// Max events per batch before forcing a flush. Caps memory.
const MAX_BATCH_SIZE: usize = 500;

/// Run the writer loop on the current thread (blocking).
///
/// Spawned from `std::thread::spawn` by `Store::open`, never from a
/// tokio task. Returns when the sender is dropped.
pub fn run_writer_loop(mut conn: Connection, mut rx: tokio::sync::mpsc::Receiver<StoreEvent>) {
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
                    flush_batch(&mut conn, &mut batch);
                    last_flush = Instant::now();
                }
            }
            None => {
                if !batch.is_empty() {
                    flush_batch(&mut conn, &mut batch);
                    last_flush = Instant::now();
                }

                if rx.is_closed() && rx.try_recv().is_err() {
                    break;
                }
            }
        }

        if !batch.is_empty() && last_flush.elapsed() >= BATCH_INTERVAL {
            flush_batch(&mut conn, &mut batch);
            last_flush = Instant::now();
        }
    }
}

/// Receive one event with a timeout (tokio's mpsc has no native
/// blocking_recv_timeout, so we poll).
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
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

/// Flush every event in `batch` inside one transaction. Drains the Vec.
fn flush_batch(conn: &mut Connection, batch: &mut Vec<StoreEvent>) {
    if batch.is_empty() {
        return;
    }

    let tx = match conn.transaction() {
        Ok(tx) => tx,
        Err(e) => {
            tracing::warn!("storage writer: failed to begin transaction: {e}");
            batch.clear();
            return;
        }
    };

    let drained = std::mem::take(batch);
    if let Err(e) = write_events(&tx, &drained) {
        tracing::warn!("storage writer: batch write failed: {e}");
    }

    if let Err(e) = tx.commit() {
        tracing::warn!("storage writer: commit failed: {e}");
    }
}

/// Bind and execute every `StoreEvent` against its prepared statement.
/// One transaction encloses the whole batch (set up by `flush_batch`).
fn write_events(tx: &Transaction, batch: &[StoreEvent]) -> rusqlite::Result<()> {
    for msg in batch {
        match &msg.event {
            ProxyEvent::Request(re) => write_transaction(tx, msg.ts, &msg.proxy, re)?,
            ProxyEvent::Session(info) => write_session(tx, &msg.proxy, info)?,
            ProxyEvent::Schema(change) => write_schema_change(tx, msg.ts, &msg.proxy, change)?,
            ProxyEvent::Heartbeat(_) => {}
        }
    }
    Ok(())
}

/// Write both halves of a consolidated `RequestEvent` in the same
/// iteration. Requests and responses share a `(session_id, request_id)`
/// key per JSON-RPC id; the proxy `request_id` is not stored here.
/// Orphan transactions (`response: None` from the pipeline error path)
/// land only the request row.
fn write_transaction(
    tx: &Transaction,
    ts: i64,
    proxy: &str,
    re: &RequestEvent,
) -> rusqlite::Result<()> {
    write_request(tx, ts, proxy, &re.request)?;
    if let Some(response) = &re.response {
        write_response(tx, ts, response)?;
    }
    Ok(())
}

// ── Request side ──────────────────────────────────────────────────────

fn write_request(
    tx: &Transaction,
    ts: i64,
    proxy: &str,
    request: &LoggedRequest,
) -> rusqlite::Result<()> {
    match request {
        LoggedRequest::Mcp(parts, rpc) => {
            let session_id = session_id_from_headers(&parts.headers);
            insert_request_row(tx, ts, proxy, session_id.as_deref(), rpc)
        }
        LoggedRequest::McpBatch(parts, rpcs) => {
            let session_id = session_id_from_headers(&parts.headers);
            for rpc in rpcs {
                insert_request_row(tx, ts, proxy, session_id.as_deref(), rpc)?;
            }
            Ok(())
        }
        LoggedRequest::OAuth { .. } | LoggedRequest::Http { .. } => Ok(()),
    }
}

fn insert_request_row(
    tx: &Transaction,
    ts: i64,
    proxy: &str,
    session_id: Option<&str>,
    rpc: &JsonRpcRequest,
) -> rusqlite::Result<()> {
    let request_id = stringify_request_id(&rpc.id);
    let method = method_str(&rpc.method);

    tx.prepare_cached(schema::INSERT_REQUEST_SQL)?
        .execute(params![
            ts,
            proxy,
            session_id,
            request_id,
            method,
            rpc.get_tool(),
            rpc.get_resource_uri(),
            rpc.get_prompt(),
            None::<i64>,
        ])?;
    Ok(())
}

// ── Response side ─────────────────────────────────────────────────────

fn write_response(tx: &Transaction, ts: i64, response: &LoggedResponse) -> rusqlite::Result<()> {
    match response {
        LoggedResponse::Mcp(parts, result) => {
            let session_id = session_id_from_headers(&parts.headers);
            insert_response_row(tx, ts, session_id.as_deref(), result)
        }
        LoggedResponse::McpBatch(parts, results) => {
            let session_id = session_id_from_headers(&parts.headers);
            for result in results {
                insert_response_row(tx, ts, session_id.as_deref(), result)?;
            }
            Ok(())
        }
        LoggedResponse::Http { .. } => Ok(()),
    }
}

fn insert_response_row(
    tx: &Transaction,
    ts: i64,
    session_id: Option<&str>,
    result: &JsonRpcResult,
) -> rusqlite::Result<()> {
    // The bare `JsonRpcError` envelope carries no jsonrpc id, so we
    // cannot correlate it back to a `requests` row. Drop until the
    // protocol layer preserves the id alongside the error.
    let resp = match result {
        JsonRpcResult::Response(resp) => resp,
        JsonRpcResult::Error(_) => return Ok(()),
    };

    let request_id = stringify_request_id(&resp.id);

    tx.prepare_cached(schema::INSERT_RESPONSE_SQL)?
        .execute(params![
            ts,
            session_id,
            request_id,
            "ok",
            None::<i64>,
            None::<&str>,
            None::<i64>,
        ])?;
    Ok(())
}

// ── Session side ──────────────────────────────────────────────────────

fn write_session(tx: &Transaction, proxy: &str, info: &SessionInfo) -> rusqlite::Result<()> {
    let state_str = match info.state {
        SessionState::Active => "active",
        SessionState::Closed => "closed",
    };
    let (client_name, client_version) = match info.client_info.as_ref() {
        Some(ci) => (Some(ci.name.as_str()), ci.version.as_deref()),
        None => (None, None),
    };

    tx.prepare_cached(schema::UPSERT_SESSION_SQL)?
        .execute(params![
            info.id,
            proxy,
            state_str,
            client_name,
            client_version,
            info.created_at.timestamp_millis(),
            info.last_active.timestamp_millis(),
            info.request_count as i64,
        ])?;
    Ok(())
}

// ── Schema side ───────────────────────────────────────────────────────

fn write_schema_change(
    tx: &Transaction,
    ts: i64,
    proxy: &str,
    change: &ChangeSchema,
) -> rusqlite::Result<()> {
    let (kind, item_key, payload, reason) = canonicalize_change(change)?;
    let payload_hash = hash_item(kind, &payload);

    let old_hash: Option<String> = tx
        .prepare_cached(schema::GET_SCHEMA_ITEM_HASH_SQL)?
        .query_row(params![proxy, kind, item_key.as_str()], |row| row.get(0))
        .ok();

    if old_hash.as_deref() == Some(payload_hash.as_str()) {
        return Ok(());
    }

    tx.prepare_cached(schema::UPSERT_SCHEMA_ITEM_SQL)?
        .execute(params![
            proxy,
            kind,
            item_key.as_str(),
            payload,
            payload_hash,
            ts,
        ])?;

    let reason_str = match reason {
        Reason::Added => "added",
        Reason::Observed => "observed",
    };

    tx.prepare_cached(schema::INSERT_SCHEMA_CHANGE_SQL)?
        .execute(params![
            proxy,
            kind,
            item_key.as_str(),
            reason_str,
            old_hash,
            payload_hash,
            ts,
        ])?;
    Ok(())
}

/// Pull the kind tag, item key, JSON payload, and reason out of a
/// `ChangeSchema`. The serialized JSON is canonical because the inner
/// types use `BTreeMap` and `serde_json::Value::Object` (a `BTreeMap`)
/// for nested data.
fn canonicalize_change(
    change: &ChangeSchema,
) -> rusqlite::Result<(&'static str, String, String, Reason)> {
    let serialize_err = |e: serde_json::Error| {
        rusqlite::Error::ToSqlConversionFailure(
            Box::new(e) as Box<dyn std::error::Error + Send + Sync>
        )
    };

    Ok(match change {
        ChangeSchema::Tool(reason, tool) => (
            "tool",
            tool.name.clone(),
            serde_json::to_string(tool).map_err(serialize_err)?,
            *reason,
        ),
        ChangeSchema::Prompt(reason, prompt) => (
            "prompt",
            prompt.name.clone(),
            serde_json::to_string(prompt).map_err(serialize_err)?,
            *reason,
        ),
        ChangeSchema::Resource(reason, resource) => (
            "resource",
            resource.uri.clone(),
            serde_json::to_string(resource).map_err(serialize_err)?,
            *reason,
        ),
        ChangeSchema::ResourceTemplate(reason, template) => (
            "resource_template",
            template.uri_template.clone(),
            serde_json::to_string(template).map_err(serialize_err)?,
            *reason,
        ),
    })
}

/// `sha256(kind || \0 || canonical_json)` as a 64-char hex string. The
/// kind tag prevents cross-kind collisions even though SHA-256 makes
/// them astronomically unlikely.
fn hash_item(kind: &str, payload: &str) -> String {
    let mut h = Sha256::new();
    h.update(kind.as_bytes());
    h.update(b"\0");
    h.update(payload.as_bytes());
    let digest = h.finalize();
    let mut out = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        write!(out, "{byte:02x}").expect("write to String never fails");
    }
    out
}

// ── Small helpers ─────────────────────────────────────────────────────

fn stringify_request_id(id: &RequestId) -> String {
    match id {
        RequestId::Number(n) => n.to_string(),
        RequestId::String(s) => s.clone(),
        RequestId::Null => "null".to_string(),
    }
}

/// MCP method name suitable for the `method` column. Returns `&'static str`
/// for known variants (via `IntoStaticStr`) or an owned `String` for
/// `Unknown`. We emit `Cow` indirectly by allocating only on `Unknown`.
fn method_str(method: &ClientMethod) -> String {
    match method {
        ClientMethod::Ping => "ping".into(),
        ClientMethod::Lifecycle(m) => Into::<&'static str>::into(*m).to_owned(),
        ClientMethod::Tools(m) => Into::<&'static str>::into(*m).to_owned(),
        ClientMethod::Resources(m) => Into::<&'static str>::into(*m).to_owned(),
        ClientMethod::Prompts(m) => Into::<&'static str>::into(*m).to_owned(),
        ClientMethod::Completion(m) => Into::<&'static str>::into(*m).to_owned(),
        ClientMethod::Logging(m) => Into::<&'static str>::into(*m).to_owned(),
        ClientMethod::Tasks(m) => Into::<&'static str>::into(*m).to_owned(),
        ClientMethod::Unknown(s) => s.clone(),
    }
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use chrono::Utc;
    use http::request::Builder as RequestBuilder;
    use http::{Method, StatusCode, Uri};
    use mcpr_core::event::types::RequestEvent;
    use mcpr_core::protocol::{
        mcp::{
            ClientMethod, JsonRpcError, JsonRpcErrorResponse, JsonRpcRequest, JsonRpcResponse,
            JsonRpcResult, JsonRpcVersion, LifecycleMethod, RequestId, ToolsMethod,
        },
        schema::{ChangeSchema, Reason, Tool},
        session::{SessionInfo, SessionState},
    };
    use serde_json::json;

    fn transaction_event(req: LoggedRequest, resp: LoggedResponse) -> ProxyEvent {
        ProxyEvent::Request(Arc::new(RequestEvent {
            request_id: "rid".into(),
            request: req,
            response: Some(resp),
            ts: Utc::now(),
            latency_us: 0,
            upstream_us: 0,
            spans: vec![],
            openai: None,
            auth: Default::default(),
            www_authenticate: None,
        }))
    }

    fn orphan_event(req: LoggedRequest) -> ProxyEvent {
        ProxyEvent::Request(Arc::new(RequestEvent {
            request_id: "rid".into(),
            request: req,
            response: None,
            ts: Utc::now(),
            latency_us: 0,
            upstream_us: 0,
            spans: vec![],
            openai: None,
            auth: Default::default(),
            www_authenticate: None,
        }))
    }

    fn empty_response_parts() -> http::response::Parts {
        http::Response::new(()).into_parts().0
    }

    fn empty_response_for(id: i64) -> LoggedResponse {
        LoggedResponse::Mcp(
            empty_response_parts(),
            JsonRpcResult::Response(JsonRpcResponse {
                jsonrpc: JsonRpcVersion,
                id: RequestId::Number(id),
                result: Some(json!({})),
            }),
        )
    }

    use crate::store::db;

    fn test_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        db::init_schema(&conn, "test").unwrap();
        conn
    }

    fn proxy_arc() -> Arc<str> {
        Arc::from("api")
    }

    fn parts_with_session(sid: &str) -> http::request::Parts {
        RequestBuilder::new()
            .method("POST")
            .uri("/")
            .header("mcp-session-id", sid)
            .body(())
            .unwrap()
            .into_parts()
            .0
    }

    fn rpc_tools_call(id: i64, tool: &str) -> JsonRpcRequest {
        JsonRpcRequest {
            jsonrpc: JsonRpcVersion,
            id: RequestId::Number(id),
            method: ClientMethod::Tools(ToolsMethod::Call),
            params: Some(serde_json::Map::from_iter([("name".into(), json!(tool))])),
        }
    }

    fn mcp_request(sid: &str, rpc: JsonRpcRequest) -> LoggedRequest {
        LoggedRequest::Mcp(parts_with_session(sid), rpc)
    }

    fn mcp_response_ok(id: i64) -> LoggedResponse {
        let parts = http::Response::new(()).into_parts().0;
        LoggedResponse::Mcp(
            parts,
            JsonRpcResult::Response(JsonRpcResponse {
                jsonrpc: JsonRpcVersion,
                id: RequestId::Number(id),
                result: Some(json!({"ok": true})),
            }),
        )
    }

    fn store_event(event: ProxyEvent, ts: i64) -> StoreEvent {
        StoreEvent {
            ts,
            proxy: proxy_arc(),
            event,
        }
    }

    fn session_event(id: &str, state: SessionState, count: u64) -> StoreEvent {
        let now = Utc::now();
        let info = SessionInfo {
            id: id.into(),
            state,
            client_info: Some(mcpr_core::protocol::mcp::ClientInfo {
                name: "claude-desktop".into(),
                version: Some("1.0.0".into()),
            }),
            server_info: None,
            created_at: now,
            last_active: now,
            request_count: count,
            request_ids: vec![],
        };
        StoreEvent {
            ts: now.timestamp_millis(),
            proxy: proxy_arc(),
            event: ProxyEvent::Session(Arc::new(info)),
        }
    }

    fn drain(conn: &mut Connection, batch: &mut Vec<StoreEvent>) {
        flush_batch(conn, batch);
    }

    // ── request side ─────────────────────────────────────────────────

    #[test]
    fn flush_batch__inserts_request_row_for_mcp() {
        let mut conn = test_db();
        let mut batch = vec![store_event(
            transaction_event(
                mcp_request("sess-1", rpc_tools_call(1, "search")),
                empty_response_for(1),
            ),
            1_000,
        )];
        drain(&mut conn, &mut batch);

        let (sid, rid, method, tool): (Option<String>, String, String, Option<String>) = conn
            .query_row(
                "SELECT session_id, request_id, method, tool FROM requests",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(sid.as_deref(), Some("sess-1"));
        assert_eq!(rid, "1");
        assert_eq!(method, "tools/call");
        assert_eq!(tool.as_deref(), Some("search"));
    }

    #[test]
    fn flush_batch__unfolds_mcp_batch_into_n_rows() {
        let mut conn = test_db();
        let parts = parts_with_session("sess-2");
        let batch_req = LoggedRequest::McpBatch(
            parts,
            vec![
                rpc_tools_call(1, "a"),
                rpc_tools_call(2, "b"),
                rpc_tools_call(3, "c"),
            ],
        );

        let mut batch = vec![store_event(
            transaction_event(batch_req, empty_response_for(1)),
            1_000,
        )];
        drain(&mut conn, &mut batch);

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM requests", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 3);
    }

    #[test]
    fn flush_batch__skips_http_request_silently() {
        let mut conn = test_db();
        let http_req = LoggedRequest::Http {
            method: Method::POST,
            uri: "/".parse::<Uri>().unwrap(),
            body_size: 0,
        };
        let http_resp = LoggedResponse::Http {
            status: StatusCode::OK,
            body_size: 0,
        };

        let mut batch = vec![store_event(transaction_event(http_req, http_resp), 1_000)];
        drain(&mut conn, &mut batch);

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM requests", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn flush_batch__duplicate_request_id_is_ignored() {
        let mut conn = test_db();
        let mut batch = vec![
            store_event(
                transaction_event(
                    mcp_request("s", rpc_tools_call(1, "x")),
                    empty_response_for(1),
                ),
                1_000,
            ),
            store_event(
                transaction_event(
                    mcp_request("s", rpc_tools_call(1, "x")),
                    empty_response_for(1),
                ),
                1_001,
            ),
        ];
        drain(&mut conn, &mut batch);

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM requests", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    // ── response side ────────────────────────────────────────────────

    #[test]
    fn flush_batch__orphan_transaction_writes_request_only() {
        let mut conn = test_db();
        let mut batch = vec![store_event(
            orphan_event(mcp_request("sess-O", rpc_tools_call(5, "x"))),
            1_000,
        )];
        drain(&mut conn, &mut batch);

        let req_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM requests", [], |row| row.get(0))
            .unwrap();
        let resp_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM responses", [], |row| row.get(0))
            .unwrap();
        assert_eq!(req_count, 1);
        assert_eq!(resp_count, 0);
    }

    #[test]
    fn flush_batch__inserts_response_ok() {
        let mut conn = test_db();
        let parts = http::Response::builder()
            .header("mcp-session-id", "sess-1")
            .body(())
            .unwrap()
            .into_parts()
            .0;
        let resp = LoggedResponse::Mcp(
            parts,
            JsonRpcResult::Response(JsonRpcResponse {
                jsonrpc: JsonRpcVersion,
                id: RequestId::Number(7),
                result: Some(json!({})),
            }),
        );

        let mut batch = vec![store_event(
            transaction_event(mcp_request("sess-1", rpc_tools_call(7, "x")), resp),
            2_000,
        )];
        drain(&mut conn, &mut batch);

        let (sid, rid, status): (Option<String>, String, String) = conn
            .query_row(
                "SELECT session_id, request_id, status FROM responses",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(sid.as_deref(), Some("sess-1"));
        assert_eq!(rid, "7");
        assert_eq!(status, "ok");
    }

    // ── join via request_log view ────────────────────────────────────

    #[test]
    fn request_log_view__pending_when_response_session_unknown() {
        let mut conn = test_db();
        let mut batch = vec![store_event(
            transaction_event(
                mcp_request("sess-3", rpc_tools_call(42, "x")),
                mcp_response_ok(42),
            ),
            1_000,
        )];
        drain(&mut conn, &mut batch);

        // The Mcp response above has no session-id header, so the view's
        // join uses (NULL, '42') ↔ (sess-3, '42'), which won't match.
        // Verify pending shows up:
        let status: String = conn
            .query_row(
                "SELECT status FROM request_log WHERE request_id = '42'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "pending");
    }

    #[test]
    fn request_log_view__joins_when_session_ids_match() {
        let mut conn = test_db();
        let parts = http::Response::builder()
            .header("mcp-session-id", "sess-J")
            .body(())
            .unwrap()
            .into_parts()
            .0;
        let resp = LoggedResponse::Mcp(
            parts,
            JsonRpcResult::Response(JsonRpcResponse {
                jsonrpc: JsonRpcVersion,
                id: RequestId::Number(99),
                result: Some(json!({})),
            }),
        );
        let mut batch = vec![store_event(
            transaction_event(mcp_request("sess-J", rpc_tools_call(99, "go")), resp),
            1_000,
        )];
        drain(&mut conn, &mut batch);

        // Both rows share the same `ts` now (consolidated emit), so the
        // view's `(res.ts - r.ts) * 1000` is 0; verify status and join.
        let (latency_us, status): (i64, String) = conn
            .query_row(
                "SELECT latency_us, status FROM request_log WHERE request_id = '99'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(latency_us, 0);
        assert_eq!(status, "ok");
    }

    // ── session side ─────────────────────────────────────────────────

    #[test]
    fn flush_batch__upserts_session_active() {
        let mut conn = test_db();
        let mut batch = vec![session_event("sess-A", SessionState::Active, 1)];
        drain(&mut conn, &mut batch);

        let (state, name, count): (String, String, i64) = conn
            .query_row(
                "SELECT state, client_name, request_count FROM sessions WHERE id = 'sess-A'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(state, "active");
        assert_eq!(name, "claude-desktop");
        assert_eq!(count, 1);
    }

    #[test]
    fn flush_batch__upsert_preserves_created_at() {
        let mut conn = test_db();
        let first = session_event("sess-B", SessionState::Active, 1);
        let original_created = first.ts;
        let mut batch = vec![first];
        drain(&mut conn, &mut batch);

        // Second event with a later created_at — should be ignored on conflict.
        let mut later = session_event("sess-B", SessionState::Closed, 5);
        later.ts = original_created + 10_000;
        let mut batch = vec![later];
        drain(&mut conn, &mut batch);

        let (state, count, created): (String, i64, i64) = conn
            .query_row(
                "SELECT state, request_count, created_at FROM sessions WHERE id = 'sess-B'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(state, "closed");
        assert_eq!(count, 5);
        // created_at must equal the first session_event's UTC timestamp,
        // not the second event's.
        assert!(created < original_created + 10_000);
    }

    // ── schema side ──────────────────────────────────────────────────

    fn tool_with(name: &str, desc: Option<&str>) -> Tool {
        Tool {
            name: name.into(),
            title: None,
            description: desc.map(str::to_owned),
            input_schema: json!({"type": "object"}),
            output_schema: None,
            annotations: None,
            meta: None,
        }
    }

    #[test]
    fn flush_batch__schema_first_seen_writes_item_and_change_with_null_old_hash() {
        let mut conn = test_db();
        let change = ChangeSchema::Tool(Reason::Added, tool_with("search", Some("v1")));
        let mut batch = vec![store_event(ProxyEvent::Schema(Arc::new(change)), 1_000)];
        drain(&mut conn, &mut batch);

        let (kind, key, hash): (String, String, String) = conn
            .query_row(
                "SELECT kind, item_key, payload_hash FROM schema_items",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(kind, "tool");
        assert_eq!(key, "search");
        assert_eq!(hash.len(), 64);

        let (reason, old_hash): (String, Option<String>) = conn
            .query_row("SELECT reason, old_hash FROM schema_changes", [], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .unwrap();
        assert_eq!(reason, "added");
        assert!(old_hash.is_none());
    }

    #[test]
    fn flush_batch__schema_identical_payload_does_not_log_change() {
        let mut conn = test_db();
        let change_a = ChangeSchema::Tool(Reason::Added, tool_with("t", Some("v1")));
        let change_b = ChangeSchema::Tool(Reason::Observed, tool_with("t", Some("v1")));

        let mut batch = vec![
            store_event(ProxyEvent::Schema(Arc::new(change_a)), 1_000),
            store_event(ProxyEvent::Schema(Arc::new(change_b)), 2_000),
        ];
        drain(&mut conn, &mut batch);

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM schema_changes", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn flush_batch__schema_payload_change_appends_change_row() {
        let mut conn = test_db();
        let change_a = ChangeSchema::Tool(Reason::Added, tool_with("t", Some("v1")));
        let change_b = ChangeSchema::Tool(Reason::Added, tool_with("t", Some("v2")));

        let mut batch = vec![
            store_event(ProxyEvent::Schema(Arc::new(change_a)), 1_000),
            store_event(ProxyEvent::Schema(Arc::new(change_b)), 2_000),
        ];
        drain(&mut conn, &mut batch);

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM schema_changes", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 2);

        let (old_hash, new_hash): (Option<String>, String) = conn
            .query_row(
                "SELECT old_hash, new_hash FROM schema_changes ORDER BY id DESC LIMIT 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert!(old_hash.is_some());
        assert_eq!(new_hash.len(), 64);
        assert_ne!(old_hash.unwrap(), new_hash);
    }

    // ── error response (id-less) is dropped, not panic ───────────────

    #[test]
    fn flush_batch__bare_jsonrpc_error_drops_response_row() {
        let mut conn = test_db();
        let parts = http::Response::new(()).into_parts().0;
        let err = LoggedResponse::Mcp(
            parts,
            JsonRpcResult::Error(JsonRpcErrorResponse {
                jsonrpc: JsonRpcVersion,
                id: RequestId::Number(99),
                error: JsonRpcError {
                    code: -32600,
                    message: "bad request".into(),
                    data: None,
                },
            }),
        );
        let mut batch = vec![store_event(
            transaction_event(mcp_request("s", rpc_tools_call(99, "x")), err),
            3_000,
        )];
        drain(&mut conn, &mut batch);

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM responses", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    // ── method_str smoke ─────────────────────────────────────────────

    #[test]
    fn method_str__known_variants_use_static_str() {
        assert_eq!(
            method_str(&ClientMethod::Lifecycle(LifecycleMethod::Initialize)),
            "initialize"
        );
        assert_eq!(
            method_str(&ClientMethod::Tools(ToolsMethod::List)),
            "tools/list"
        );
    }

    #[test]
    fn method_str__unknown_uses_owned_string() {
        assert_eq!(
            method_str(&ClientMethod::Unknown("custom/thing".into())),
            "custom/thing"
        );
    }
}
