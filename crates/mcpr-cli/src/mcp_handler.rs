use std::time::Instant;

use axum::{
    body::{Body, Bytes},
    http::{HeaderMap, Method, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde_json::Value;

use crate::AppState;
use crate::proxy::forward_request;
use crate::widgets::fetch_widget_html;
use mcpr_core::event::{
    ProxyEvent, RequestEvent, SchemaCaptureEvent, SchemaStaleEvent, SessionStartEvent,
};
use mcpr_protocol::schema as proto_schema;
use mcpr_protocol::session::{self as session, SessionState, SessionStore};
use mcpr_protocol::{self as jsonrpc, McpMethod};
use mcpr_proxy::forwarding::{build_response, read_body_capped};
use mcpr_proxy::rewrite_response;
use mcpr_proxy::sse::{extract_json_from_sse, wrap_as_sse};

/// Normalize a client name to a platform identifier.
fn normalize_platform(client_name: &str) -> &'static str {
    let lower = client_name.to_lowercase();
    if lower.contains("claude") {
        "claude"
    } else if lower.contains("cursor") {
        "cursor"
    } else if lower.contains("chatgpt") || lower.contains("openai") {
        "chatgpt"
    } else if lower.contains("copilot") || lower.contains("vscode") || lower.contains("vs-code") {
        "vscode"
    } else if lower.contains("windsurf") {
        "windsurf"
    } else {
        "unknown"
    }
}

/// Build a `ProxyEvent::Request` from the request/response data.
#[allow(clippy::too_many_arguments)]
fn make_request_event(
    proxy: &str,
    path: &str,
    method_str: &str,
    mcp_method: &McpMethod,
    call_detail: &Option<String>,
    session_id: Option<&str>,
    status: u16,
    start: Instant,
    upstream_us: Option<u64>,
    request_size: usize,
    response_size: Option<usize>,
    rpc_error: Option<&(i64, String)>,
    note: &str,
) -> ProxyEvent {
    let tool = if *mcp_method == McpMethod::ToolsCall {
        call_detail.clone()
    } else {
        None
    };

    ProxyEvent::Request(RequestEvent {
        id: uuid::Uuid::new_v4().to_string(),
        ts: chrono::Utc::now().timestamp_millis(),
        proxy: proxy.to_string(),
        session_id: session_id.map(String::from),
        method: "POST".to_string(),
        path: path.to_string(),
        mcp_method: Some(method_str.to_string()),
        tool,
        status,
        latency_us: start.elapsed().as_micros() as u64,
        upstream_us,
        request_size: Some(request_size as u64),
        response_size: response_size.map(|s| s as u64),
        error_code: rpc_error.map(|(code, _)| code.to_string()),
        error_msg: rpc_error.map(|(_, msg)| msg.chars().take(512).collect()),
        note: note.to_string(),
    })
}

/// Handle MCP JSON-RPC POST — intercept resources/read, forward, rewrite response.
pub async fn handle_mcp_post(
    state: &AppState,
    path: &str,
    headers: &HeaderMap,
    body: &Bytes,
    parsed: jsonrpc::ParsedBody,
    start: Instant,
) -> Response {
    let raw_body_len = body.len();
    let mcp_method = parsed.mcp_method();
    let method_str = mcp_method.as_str();
    let call_detail = parsed.detail();

    // Extract client info from initialize request before forwarding
    let client_info = if mcp_method == McpMethod::Initialize {
        parsed.first_params().and_then(session::parse_client_info)
    } else {
        None
    };

    // Track session activity and state transitions
    let req_session_id = headers
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    if let Some(ref sid) = req_session_id {
        state.sessions.touch(sid).await;
        if mcp_method == McpMethod::Initialized {
            state.sessions.update_state(sid, SessionState::Active).await;
        }
    }

    // Intercept resources/read for widget HTML serving (single requests only)
    if mcp_method == McpMethod::ResourcesRead
        && !parsed.is_batch
        && state.widget_source.is_some()
        && let Ok(json_val) = serde_json::from_slice::<Value>(body)
        && let Some(response) = handle_resources_read(state, headers, body, &json_val, start).await
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

            // Single emit — all sinks (stderr, sqlite, cloud) receive this.
            state.event_bus.emit(make_request_event(
                &state.proxy_name,
                path,
                method_str,
                &mcp_method,
                &call_detail,
                req_session_id.as_deref(),
                502,
                start,
                Some(upstream_us),
                raw_body_len,
                None,
                None,
                "upstream error",
            ));

            return (StatusCode::BAD_GATEWAY, format!("Upstream error: {e}")).into_response();
        }
    };

    let status = resp.status().as_u16();
    let resp_headers = resp.headers().clone();

    // Track session from upstream response
    let resp_session_id = resp_headers
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .map(String::from);

    if mcp_method == McpMethod::Initialize && status < 400 {
        mcpr_proxy::lock_state(&state.proxy_state_ref).confirm_mcp_connected();
    }

    // On successful initialize, emit SessionStart event
    if mcp_method == McpMethod::Initialize
        && status < 400
        && let Some(ref sid) = resp_session_id
    {
        state.sessions.create(sid).await;
        state
            .sessions
            .update_state(sid, SessionState::Initialized)
            .await;

        let (client_name, client_version, client_platform) = if let Some(info) = client_info {
            let platform = normalize_platform(&info.name).to_string();
            let name = info.name.clone();
            let version = info.version.clone();
            state.sessions.set_client_info(sid, info).await;
            (Some(name), version, Some(platform))
        } else {
            (None, None, None)
        };

        state
            .event_bus
            .emit(ProxyEvent::SessionStart(SessionStartEvent {
                session_id: sid.clone(),
                proxy: state.proxy_name.clone(),
                ts: chrono::Utc::now().timestamp_millis(),
                client_name,
                client_version,
                client_platform,
            }));
    }

    let log_session_id = resp_session_id.or(req_session_id);

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
        let rpc_error =
            jsonrpc::extract_error_code(&json_body).map(|(code, msg)| (code, msg.to_string()));

        // Schema capture — BEFORE rewrite, to get raw server response.
        emit_schema_capture(state, &mcp_method, method_str, body, &json_body);
        emit_schema_stale(state, &json_body);

        rewrite_response(method_str, &mut json_body, &config);
        let rewritten = serde_json::to_vec(&json_body).unwrap_or(json_bytes);
        let body = if is_sse {
            wrap_as_sse(&rewritten)
        } else {
            rewritten
        };
        let note = if is_sse { "rewritten+sse" } else { "rewritten" };

        // Single emit — replaces logger.emit + events.emit + store.record
        state.event_bus.emit(make_request_event(
            &state.proxy_name,
            path,
            method_str,
            &mcp_method,
            &call_detail,
            log_session_id.as_deref(),
            status,
            start,
            Some(upstream_us),
            raw_body_len,
            Some(body.len()),
            rpc_error.as_ref(),
            note,
        ));

        build_response(status, &resp_headers, Body::from(body))
    } else {
        // Non-JSON response — passthrough
        state.event_bus.emit(make_request_event(
            &state.proxy_name,
            path,
            method_str,
            &mcp_method,
            &call_detail,
            log_session_id.as_deref(),
            status,
            start,
            Some(upstream_us),
            raw_body_len,
            Some(resp_bytes.len()),
            None,
            "passthrough",
        ));

        build_response(status, &resp_headers, Body::from(resp_bytes))
    }
}

/// Handle MCP SSE GET — stream from upstream.
pub async fn handle_mcp_sse(
    state: &AppState,
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

            state.event_bus.emit(ProxyEvent::Request(RequestEvent {
                id: uuid::Uuid::new_v4().to_string(),
                ts: chrono::Utc::now().timestamp_millis(),
                proxy: state.proxy_name.clone(),
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
                note: "sse".to_string(),
            }));

            build_response(
                status,
                &resp_headers,
                Body::from_stream(resp.bytes_stream()),
            )
        }
        Err(e) => {
            state.event_bus.emit(ProxyEvent::Request(RequestEvent {
                id: uuid::Uuid::new_v4().to_string(),
                ts: chrono::Utc::now().timestamp_millis(),
                proxy: state.proxy_name.clone(),
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
                note: "upstream error".to_string(),
            }));

            (StatusCode::BAD_GATEWAY, format!("Upstream error: {e}")).into_response()
        }
    }
}

/// Handle resources/read interception: serve local widget HTML + upstream metadata.
async fn handle_resources_read(
    state: &AppState,
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

    state.event_bus.emit(ProxyEvent::Request(RequestEvent {
        id: uuid::Uuid::new_v4().to_string(),
        ts: chrono::Utc::now().timestamp_millis(),
        proxy: state.proxy_name.clone(),
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
        note: "intercepted".to_string(),
    }));

    let mut resp_headers = HeaderMap::new();
    resp_headers.insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
    Some(build_response(200, &resp_headers, Body::from(body)))
}

// ── Schema capture helpers ───────────────────────────────────────────

/// Emit a schema capture event if this is a successful schema discovery response.
fn emit_schema_capture(
    state: &AppState,
    mcp_method: &McpMethod,
    method_str: &str,
    request_body: &Bytes,
    response_body: &Value,
) {
    if !proto_schema::is_schema_method(mcp_method) {
        return;
    }

    // Only capture successful responses that have a result field.
    let result = match response_body.get("result") {
        Some(r) => r,
        None => return,
    };

    let req_val = serde_json::from_slice::<Value>(request_body).unwrap_or_default();
    let page_status = proto_schema::detect_page_status(&req_val, response_body);
    let payload = result.to_string();

    state
        .event_bus
        .emit(ProxyEvent::SchemaCapture(SchemaCaptureEvent {
            ts: chrono::Utc::now().timestamp_millis(),
            proxy: state.proxy_name.clone(),
            upstream_url: state.mcp_upstream.clone(),
            method: method_str.to_string(),
            payload,
            page_status,
        }));
}

/// Check if the response contains a `notifications/tools/list_changed` notification
/// and emit a schema stale event if so.
fn emit_schema_stale(state: &AppState, response_body: &Value) {
    let is_stale_notification = |v: &Value| {
        v.get("method").and_then(|m| m.as_str()) == Some(jsonrpc::NOTIFICATIONS_TOOLS_LIST_CHANGED)
    };

    // Check single notification.
    if is_stale_notification(response_body) {
        state
            .event_bus
            .emit(ProxyEvent::SchemaStale(SchemaStaleEvent {
                ts: chrono::Utc::now().timestamp_millis(),
                proxy: state.proxy_name.clone(),
                upstream_url: state.mcp_upstream.clone(),
                method: "tools/list".to_string(),
            }));
        return;
    }

    // Check batch containing the notification.
    if let Some(arr) = response_body.as_array()
        && arr.iter().any(is_stale_notification)
    {
        state
            .event_bus
            .emit(ProxyEvent::SchemaStale(SchemaStaleEvent {
                ts: chrono::Utc::now().timestamp_millis(),
                proxy: state.proxy_name.clone(),
                upstream_url: state.mcp_upstream.clone(),
                method: "tools/list".to_string(),
            }));
    }
}
