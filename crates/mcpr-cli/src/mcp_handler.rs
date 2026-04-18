use std::time::Instant;

use axum::{
    body::{Body, Bytes},
    http::{HeaderMap, Method, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde_json::Value;

use crate::pipeline::{ResponseSummary, RequestContext, emit_request_event, normalize_platform};
use crate::proxy::forward_request;
use crate::state::ProxyState;
use crate::widgets::fetch_widget_html;
use mcpr_core::event::{ProxyEvent, RequestEvent, SchemaVersionCreatedEvent, SessionStartEvent};
use mcpr_core::protocol::schema as proto_schema;
use mcpr_core::protocol::session::{SessionState, SessionStore};
use mcpr_core::protocol::{self as jsonrpc, McpMethod};
use mcpr_core::proxy::forwarding::{build_response, read_body_capped};
use mcpr_core::proxy::rewrite_response;
use mcpr_core::proxy::sse::{extract_json_from_sse, wrap_as_sse};

/// Handle MCP JSON-RPC POST — intercept resources/read, forward, rewrite response.
pub async fn handle_mcp_post(
    state: &ProxyState,
    ctx: &mut RequestContext,
    headers: &HeaderMap,
    body: &Bytes,
) -> Response {
    let mcp_method = ctx
        .mcp_method
        .clone()
        .expect("handle_mcp_post called without an MCP method");
    let method_str: String = ctx
        .mcp_method_str
        .clone()
        .expect("handle_mcp_post called without a parsed method string");

    // Track session activity and state transitions
    if let Some(ref sid) = ctx.session_id {
        state.sessions.touch(sid).await;
        if mcp_method == McpMethod::Initialized {
            state.sessions.update_state(sid, SessionState::Active).await;
        }
    }

    // Intercept resources/read for widget HTML serving (single requests only)
    if mcp_method == McpMethod::ResourcesRead
        && !ctx.is_batch
        && state.widget_source.is_some()
        && let Ok(json_val) = serde_json::from_slice::<Value>(body)
        && let Some(response) =
            handle_resources_read(state, headers, body, &json_val, ctx.start).await
    {
        return response;
    }

    // Forward to upstream MCP URL
    let upstream_url = state.mcp_upstream.trim_end_matches('/').to_string();
    let upstream_start = Instant::now();
    let resp = match forward_request(state, &upstream_url, Method::POST, headers, body, false).await
    {
        Ok(r) => r,
        Err(e) => {
            let upstream_us = upstream_start.elapsed().as_micros() as u64;

            // Resolve client info from the request-side session (upstream
            // didn't respond — no resp session id to merge in).
            populate_client_info(state, ctx).await;

            emit_request_event(
                state,
                ctx,
                &ResponseSummary {
                    status: 502,
                    response_size: None,
                    upstream_us: Some(upstream_us),
                    error_code: None,
                    error_msg: None,
                },
                "upstream error",
            );

            return (StatusCode::BAD_GATEWAY, format!("Upstream error: {e}")).into_response();
        }
    };

    let status = resp.status().as_u16();
    let resp_headers = resp.headers().clone();

    // Track session id from upstream response — overwrites ctx's request-side
    // id so downstream logic (SessionStart, emit) sees the authoritative one.
    if let Some(resp_sid) = resp_headers
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
    {
        ctx.session_id = Some(resp_sid.to_string());
    }

    if mcp_method == McpMethod::Initialize && status < 400 {
        mcpr_core::proxy::lock_health(&state.health).confirm_mcp_connected();
    }

    // On successful initialize, store client info in the session + emit SessionStart.
    if mcp_method == McpMethod::Initialize
        && status < 400
        && let Some(sid) = ctx.session_id.clone()
    {
        state.sessions.create(&sid).await;
        state
            .sessions
            .update_state(&sid, SessionState::Initialized)
            .await;

        let (client_name, client_version, client_platform) =
            if let Some(info) = ctx.client_info_from_init.take() {
                let platform = normalize_platform(&info.name).to_string();
                let name = info.name.clone();
                let version = info.version.clone();
                state.sessions.set_client_info(&sid, info).await;
                (Some(name), version, Some(platform))
            } else {
                (None, None, None)
            };

        state
            .event_bus
            .emit(ProxyEvent::SessionStart(SessionStartEvent {
                session_id: sid,
                proxy: state.name.clone(),
                ts: chrono::Utc::now().timestamp_millis(),
                client_name,
                client_version,
                client_platform,
            }));
    }

    // Resolve client info for emit from the (possibly-just-written) session.
    populate_client_info(state, ctx).await;

    // Collect full body for rewriting (POST SSE is finite), capped to prevent OOM
    let resp_bytes = match read_body_capped(resp, state.max_response_body).await {
        Ok(b) => b,
        Err(err_resp) => return err_resp,
    };
    let upstream_us = upstream_start.elapsed().as_micros() as u64;
    let config = state.rewrite_config.read().await;

    // Try to parse and rewrite JSON response (may be SSE-wrapped)
    let (json_bytes, is_sse) = match extract_json_from_sse(&resp_bytes) {
        Some(extracted) => (extracted, true),
        None => (resp_bytes.to_vec(), false),
    };

    if let Ok(mut json_body) = serde_json::from_slice::<Value>(&json_bytes) {
        let rpc_error = jsonrpc::extract_error_code(&json_body)
            .map(|(code, msg)| (code, msg.to_string()));

        // Schema ingest — BEFORE rewrite, so the raw server response is
        // what gets hashed + persisted. `ingest` returns `Some(version)`
        // only when pagination is complete AND the content changed.
        ingest_schema(state, &mcp_method, &method_str, body, &json_body).await;
        observe_schema_stale(state, &json_body);

        rewrite_response(&method_str, &mut json_body, &config);
        let rewritten = serde_json::to_vec(&json_body).unwrap_or(json_bytes);
        let body = if is_sse {
            wrap_as_sse(&rewritten)
        } else {
            rewritten
        };
        let note = if is_sse { "rewritten+sse" } else { "rewritten" };

        let mut summary = ResponseSummary {
            status,
            response_size: Some(body.len() as u64),
            upstream_us: Some(upstream_us),
            error_code: None,
            error_msg: None,
        };
        if let Some((code, msg)) = rpc_error {
            summary = summary.with_rpc_error(code, msg);
        }
        emit_request_event(state, ctx, &summary, note);

        build_response(status, &resp_headers, Body::from(body))
    } else {
        // Non-JSON response — passthrough
        emit_request_event(
            state,
            ctx,
            &ResponseSummary {
                status,
                response_size: Some(resp_bytes.len() as u64),
                upstream_us: Some(upstream_us),
                error_code: None,
                error_msg: None,
            },
            "passthrough",
        );

        build_response(status, &resp_headers, Body::from(resp_bytes))
    }
}

/// Look up client name/version from the session store (if a session id is
/// known) and stash them on the context so `emit_request_event` picks them up.
async fn populate_client_info(state: &ProxyState, ctx: &mut RequestContext) {
    if let Some(ref sid) = ctx.session_id
        && let Some(info) = state.sessions.get(sid).await
        && let Some(ci) = info.client_info
    {
        ctx.client_name = Some(ci.name);
        ctx.client_version = ci.version;
    }
}

/// Handle MCP SSE GET — stream from upstream.
pub async fn handle_mcp_sse(
    state: &ProxyState,
    path: &str,
    headers: &HeaderMap,
    start: Instant,
) -> Response {
    let upstream_url = state.mcp_upstream.trim_end_matches('/').to_string();
    let upstream_start = Instant::now();
    match forward_request(
        state,
        &upstream_url,
        Method::GET,
        headers,
        &Bytes::new(),
        true,
    )
    .await
    {
        Ok(resp) => {
            let status = resp.status().as_u16();
            let resp_headers = resp.headers().clone();

            state
                .event_bus
                .emit(ProxyEvent::Request(Box::new(RequestEvent {
                    id: uuid::Uuid::new_v4().to_string(),
                    ts: chrono::Utc::now().timestamp_millis(),
                    proxy: state.name.clone(),
                    session_id: None,
                    method: "GET".to_string(),
                    path: path.to_string(),
                    mcp_method: Some("SSE".to_string()),
                    tool: None,
                    status,
                    latency_us: start.elapsed().as_micros() as u64,
                    upstream_us: Some(upstream_start.elapsed().as_micros() as u64),
                    request_size: None,
                    response_size: None,
                    error_code: None,
                    error_msg: None,
                    client_name: None,
                    client_version: None,
                    note: "sse".to_string(),
                })));

            build_response(
                status,
                &resp_headers,
                Body::from_stream(resp.bytes_stream()),
            )
        }
        Err(e) => {
            state
                .event_bus
                .emit(ProxyEvent::Request(Box::new(RequestEvent {
                    id: uuid::Uuid::new_v4().to_string(),
                    ts: chrono::Utc::now().timestamp_millis(),
                    proxy: state.name.clone(),
                    session_id: None,
                    method: "GET".to_string(),
                    path: path.to_string(),
                    mcp_method: Some("SSE".to_string()),
                    tool: None,
                    status: 502,
                    latency_us: start.elapsed().as_micros() as u64,
                    upstream_us: Some(upstream_start.elapsed().as_micros() as u64),
                    request_size: None,
                    response_size: None,
                    error_code: None,
                    error_msg: Some(format!("{e}").chars().take(512).collect()),
                    client_name: None,
                    client_version: None,
                    note: "upstream error".to_string(),
                })));

            (StatusCode::BAD_GATEWAY, format!("Upstream error: {e}")).into_response()
        }
    }
}

/// Handle resources/read interception: serve local widget HTML + upstream metadata.
async fn handle_resources_read(
    state: &ProxyState,
    headers: &HeaderMap,
    raw_body: &Bytes,
    parsed: &Value,
    start: Instant,
) -> Option<Response> {
    let uri = parsed
        .get("params")
        .and_then(|p| p.get("uri"))
        .and_then(|u| u.as_str())?;

    let widget_name = uri.strip_prefix("ui://widget/")?;
    let widget_name = widget_name.trim_end_matches(".html");

    let html = fetch_widget_html(state, widget_name).await?;

    let upstream_url = state.mcp_upstream.trim_end_matches('/').to_string();
    let upstream_start = Instant::now();
    let upstream_resp =
        forward_request(state, &upstream_url, Method::POST, headers, raw_body, false)
            .await
            .ok()?;

    let upstream_bytes = read_body_capped(upstream_resp, state.max_response_body)
        .await
        .ok()?;
    let upstream_us = upstream_start.elapsed().as_micros() as u64;
    let json_bytes =
        extract_json_from_sse(&upstream_bytes).unwrap_or_else(|| upstream_bytes.to_vec());
    let mut json_body: Value = serde_json::from_slice(&json_bytes).ok()?;

    if let Some(contents) = json_body
        .get_mut("result")
        .and_then(|r| r.get_mut("contents"))
        .and_then(|c| c.as_array_mut())
    {
        for content in contents.iter_mut() {
            if content.get("text").is_some() {
                content["text"] = Value::String(html.clone());
            }
        }
    }

    let config = state.rewrite_config.read().await;
    rewrite_response(jsonrpc::RESOURCES_READ, &mut json_body, &config);
    drop(config);

    let body = serde_json::to_vec(&json_body).unwrap_or_default();

    state
        .event_bus
        .emit(ProxyEvent::Request(Box::new(RequestEvent {
            id: uuid::Uuid::new_v4().to_string(),
            ts: chrono::Utc::now().timestamp_millis(),
            proxy: state.name.clone(),
            session_id: None,
            method: "POST".to_string(),
            path: "/*".to_string(),
            mcp_method: Some(jsonrpc::RESOURCES_READ.to_string()),
            tool: None,
            status: 200,
            latency_us: start.elapsed().as_micros() as u64,
            upstream_us: Some(upstream_us),
            request_size: Some(raw_body.len() as u64),
            response_size: Some(body.len() as u64),
            error_code: None,
            error_msg: None,
            client_name: None,
            client_version: None,
            note: "intercepted".to_string(),
        })));

    let mut resp_headers = HeaderMap::new();
    resp_headers.insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
    Some(build_response(200, &resp_headers, Body::from(body)))
}

// ── Schema ingest helpers ────────────────────────────────────────────

/// Feed a schema-method response into the `SchemaManager` and emit
/// `SchemaVersionCreated` if a new version was produced.
async fn ingest_schema(
    state: &ProxyState,
    mcp_method: &McpMethod,
    method_str: &str,
    request_body: &Bytes,
    response_body: &Value,
) {
    if !proto_schema::is_schema_method(mcp_method) {
        return;
    }
    let req_val = serde_json::from_slice::<Value>(request_body).unwrap_or_default();
    let Some(version) = state
        .schema_manager
        .ingest(method_str, &req_val, response_body)
        .await
    else {
        return;
    };

    state.event_bus.emit(ProxyEvent::SchemaVersionCreated(
        SchemaVersionCreatedEvent {
            ts: chrono::Utc::now().timestamp_millis(),
            upstream_id: state.name.clone(),
            upstream_url: state.mcp_upstream.clone(),
            method: version.method.clone(),
            version: version.version,
            version_id: version.id.to_string(),
            content_hash: version.content_hash.clone(),
            payload: (*version.payload).clone(),
        },
    ));
}

/// True if `response_body` carries a `notifications/tools/list_changed`
/// notification — either as a single JSON-RPC message or inside a batch
/// array.
fn is_list_changed_response(response_body: &Value) -> bool {
    let is_notif = |v: &Value| {
        v.get("method").and_then(|m| m.as_str()) == Some(jsonrpc::NOTIFICATIONS_TOOLS_LIST_CHANGED)
    };
    is_notif(response_body)
        || response_body
            .as_array()
            .is_some_and(|arr| arr.iter().any(is_notif))
}

/// Mark the cached `tools/list` schema stale if the response carries
/// a `notifications/tools/list_changed` notification.
fn observe_schema_stale(state: &ProxyState, response_body: &Value) {
    if is_list_changed_response(response_body) {
        state.schema_manager.mark_stale(jsonrpc::TOOLS_LIST);
    }
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;

    // ── Schema ingest / stale observation ─────────────────────────────

    use mcpr_core::protocol::schema_manager::{MemorySchemaStore, SchemaManager};
    use serde_json::json;
    use std::sync::Arc;

    fn schema_test_state() -> ProxyState {
        use tokio::sync::RwLock;
        ProxyState {
            name: "test".to_string(),
            mcp_upstream: "http://upstream:9000".to_string(),
            upstream: mcpr_core::proxy::forwarding::UpstreamClient {
                http_client: reqwest::Client::builder().build().unwrap(),
                semaphore: Arc::new(tokio::sync::Semaphore::new(10)),
                request_timeout: std::time::Duration::from_secs(30),
            },
            max_request_body: 1024 * 1024,
            max_response_body: 1024 * 1024,
            rewrite_config: Arc::new(RwLock::new(mcpr_core::proxy::RewriteConfig {
                proxy_url: "http://localhost:0".to_string(),
                proxy_domain: "localhost".to_string(),
                mcp_upstream: "http://upstream:9000".to_string(),
                csp: mcpr_core::proxy::CspConfig::default(),
            })),
            widget_source: None,
            sessions: mcpr_core::protocol::session::MemorySessionStore::new(),
            schema_manager: Arc::new(SchemaManager::new("test", MemorySchemaStore::new())),
            health: mcpr_core::proxy::new_shared_health(),
            event_bus: mcpr_core::event::EventManager::new().start().bus,
        }
    }

    #[test]
    fn is_list_changed_response__single_notification() {
        let resp = json!({"jsonrpc": "2.0", "method": "notifications/tools/list_changed"});
        assert!(is_list_changed_response(&resp));
    }

    #[test]
    fn is_list_changed_response__batch_with_notification() {
        let resp = json!([
            {"jsonrpc": "2.0", "id": 1, "result": {}},
            {"jsonrpc": "2.0", "method": "notifications/tools/list_changed"}
        ]);
        assert!(is_list_changed_response(&resp));
    }

    #[test]
    fn is_list_changed_response__unrelated_response_false() {
        let resp = json!({"jsonrpc": "2.0", "id": 1, "result": {"tools": []}});
        assert!(!is_list_changed_response(&resp));
    }

    #[test]
    fn is_list_changed_response__empty_batch_false() {
        let resp = json!([]);
        assert!(!is_list_changed_response(&resp));
    }

    #[tokio::test]
    async fn observe_schema_stale__sets_flag_on_notification() {
        let state = schema_test_state();
        assert!(!state.schema_manager.is_stale("tools/list"));

        let resp = json!({"jsonrpc": "2.0", "method": "notifications/tools/list_changed"});
        observe_schema_stale(&state, &resp);

        assert!(state.schema_manager.is_stale("tools/list"));
    }

    #[tokio::test]
    async fn observe_schema_stale__noop_on_unrelated_response() {
        let state = schema_test_state();
        let resp = json!({"jsonrpc": "2.0", "id": 1, "result": {}});
        observe_schema_stale(&state, &resp);
        assert!(!state.schema_manager.is_stale("tools/list"));
    }

    #[tokio::test]
    async fn ingest_schema__non_schema_method_is_noop() {
        let state = schema_test_state();
        let body = Bytes::from_static(br#"{"jsonrpc":"2.0","id":1,"method":"tools/call"}"#);
        let resp = json!({"jsonrpc": "2.0", "id": 1, "result": {}});
        ingest_schema(&state, &McpMethod::ToolsCall, "tools/call", &body, &resp).await;
        assert!(state.schema_manager.latest("tools/list").await.is_none());
    }

    #[tokio::test]
    async fn ingest_schema__error_response_is_noop() {
        let state = schema_test_state();
        let body = Bytes::from_static(br#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#);
        let resp = json!({"jsonrpc": "2.0", "id": 1, "error": {"code": -32603, "message": "x"}});
        ingest_schema(&state, &McpMethod::ToolsList, "tools/list", &body, &resp).await;
        assert!(state.schema_manager.latest("tools/list").await.is_none());
    }

    #[tokio::test]
    async fn ingest_schema__schema_method_creates_version() {
        let state = schema_test_state();
        let body = Bytes::from_static(br#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#);
        let resp = json!({
            "jsonrpc": "2.0", "id": 1,
            "result": {"tools": [{"name": "search"}]}
        });
        ingest_schema(&state, &McpMethod::ToolsList, "tools/list", &body, &resp).await;

        let latest = state.schema_manager.latest("tools/list").await.unwrap();
        assert_eq!(latest.version, 1);
        assert_eq!(latest.method, "tools/list");
    }

    #[tokio::test]
    async fn ingest_schema__unchanged_payload_no_new_version() {
        let state = schema_test_state();
        let body = Bytes::from_static(br#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#);
        let resp = json!({
            "jsonrpc": "2.0", "id": 1,
            "result": {"tools": [{"name": "search"}]}
        });
        ingest_schema(&state, &McpMethod::ToolsList, "tools/list", &body, &resp).await;
        ingest_schema(&state, &McpMethod::ToolsList, "tools/list", &body, &resp).await;

        let latest = state.schema_manager.latest("tools/list").await.unwrap();
        assert_eq!(latest.version, 1);
    }
}
