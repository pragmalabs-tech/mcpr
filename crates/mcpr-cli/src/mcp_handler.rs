use std::time::Instant;

use axum::{
    body::{Body, Bytes},
    http::{HeaderMap, Method, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde_json::Value;

use crate::AppState;
use crate::logger::LogEntry;
use crate::proxy::forward_request;
use crate::widgets::fetch_widget_html;
use mcpr_integrations::store::{RequestEvent, RequestStatus, SessionEvent, StoreEvent};
use mcpr_integrations::{EventStatus, EventType, McprEvent};
use mcpr_protocol::session::{self as session, SessionState, SessionStore};
use mcpr_protocol::{self as jsonrpc, McpMethod};
use mcpr_proxy::forwarding::{build_response, read_body_capped};
use mcpr_proxy::rewrite_response;
use mcpr_proxy::sse::{extract_json_from_sse, wrap_as_sse};

/// Normalize a client name to a platform identifier.
///
/// Maps known MCP client names to their platform category. Used when
/// recording sessions in the store for aggregation by `mcpr proxy clients`.
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

/// Map an MCP method to the corresponding event type.
fn event_type_for(method: &McpMethod) -> EventType {
    match method {
        McpMethod::ToolsCall => EventType::ToolCall,
        McpMethod::ToolsList => EventType::ToolList,
        McpMethod::Initialize => EventType::SessionStart,
        _ => EventType::Request,
    }
}

/// Handle MCP JSON-RPC POST — intercept resources/read, forward, rewrite response.
/// The body has already been validated as JSON-RPC 2.0 by `classify`.
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
            state.logger.emit(
                LogEntry::new("POST", path, 0, "session:active")
                    .session_id(sid)
                    .mcp_method(method_str),
            );
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
            let upstream_ms = upstream_start.elapsed().as_millis() as u64;
            state.logger.emit(
                LogEntry::new("POST", path, 502, "upstream error")
                    .mcp_method(method_str)
                    .maybe_detail(call_detail.as_deref())
                    .maybe_session_id(req_session_id.as_deref())
                    .upstream(&upstream_url)
                    .req_size(body.len())
                    .upstream_duration(upstream_ms)
                    .duration(start),
            );
            let mut evt = McprEvent::new(event_type_for(&mcp_method))
                .method(method_str)
                .upstream(&upstream_url)
                .latency(start.elapsed().as_millis() as u64)
                .upstream_ms(upstream_ms)
                .status(EventStatus::Error)
                .request_size(raw_body_len as u64)
                .error_detail(format!("{e}"));
            if let Some(ref sid) = req_session_id {
                evt = evt.session(sid);
                if let Some(info) = state.sessions.get(sid).await.and_then(|s| s.client_info) {
                    evt = evt.client_name(&info.name);
                    if let Some(ref v) = info.version {
                        evt = evt.client_version(v);
                    }
                }
            }
            if let Some(ref d) = call_detail
                && mcp_method == McpMethod::ToolsCall
            {
                evt = evt.tool(d);
            }
            state.events.emit(evt);

            // Record failed request in store.
            if let Some(ref store) = state.store {
                let tool = if mcp_method == McpMethod::ToolsCall {
                    call_detail.clone()
                } else {
                    None
                };
                store.record(StoreEvent::Request(RequestEvent {
                    request_id: uuid::Uuid::new_v4().to_string(),
                    ts: chrono::Utc::now().timestamp_millis(),
                    proxy: state.proxy_name.clone(),
                    session_id: req_session_id.clone(),
                    method: method_str.to_string(),
                    tool,
                    latency_ms: start.elapsed().as_millis() as i64,
                    status: RequestStatus::Error,
                    error_code: None,
                    error_msg: Some(format!("{e}").chars().take(512).collect()),
                    bytes_in: Some(raw_body_len as i64),
                    bytes_out: None,
                }));
            }

            return (StatusCode::BAD_GATEWAY, format!("Upstream error: {e}")).into_response();
        }
    };

    let status = resp.status().as_u16();
    let resp_headers = resp.headers().clone();

    // Track session from upstream response — for initialize, the session ID is in the response
    let resp_session_id = resp_headers
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    // On successful initialize, confirm MCP connectivity.
    if mcp_method == McpMethod::Initialize && status < 400 {
        mcpr_proxy::lock_state(&state.proxy_state_ref).confirm_mcp_connected();
    }

    if mcp_method == McpMethod::Initialize
        && status < 400
        && let Some(ref sid) = resp_session_id
    {
        state.sessions.create(sid).await;
        state
            .sessions
            .update_state(sid, SessionState::Initialized)
            .await;
        let mut session_log = LogEntry::new("POST", path, 0, "session:created")
            .session_id(sid)
            .mcp_method(method_str);
        if let Some(info) = client_info {
            let label = match &info.version {
                Some(v) => format!("{} {v}", info.name),
                None => info.name.clone(),
            };
            session_log = session_log.client_name(&label);

            // Record session in the store (before consuming client_info).
            if let Some(ref store) = state.store {
                store.record(StoreEvent::Session(SessionEvent {
                    session_id: sid.clone(),
                    proxy: state.proxy_name.clone(),
                    started_at: chrono::Utc::now().timestamp_millis(),
                    client_name: Some(info.name.clone()),
                    client_version: info.version.clone(),
                    client_platform: Some(normalize_platform(&info.name).to_string()),
                }));
            }

            state.sessions.set_client_info(sid, info).await;
        } else if let Some(ref store) = state.store {
            // No client info — still record the session.
            store.record(StoreEvent::Session(SessionEvent {
                session_id: sid.clone(),
                proxy: state.proxy_name.clone(),
                started_at: chrono::Utc::now().timestamp_millis(),
                client_name: None,
                client_version: None,
                client_platform: None,
            }));
        }
        state.logger.emit(session_log);
    }
    // Use response session ID (for initialize) or request session ID (for everything else)
    let log_session_id = resp_session_id.or(req_session_id);

    // Collect full body for rewriting (POST SSE is finite), capped to prevent OOM
    let resp_bytes = match read_body_capped(resp, state.max_response_body).await {
        Ok(b) => b,
        Err(err_resp) => return err_resp,
    };
    let upstream_ms = upstream_start.elapsed().as_millis() as u64;
    let config = state.rewrite_config.read().await;

    // Try to parse and rewrite JSON response (may be SSE-wrapped)
    let (json_bytes, is_sse) = match extract_json_from_sse(&resp_bytes) {
        Some(extracted) => (extracted, true),
        None => (resp_bytes.to_vec(), false),
    };

    if let Ok(mut json_body) = serde_json::from_slice::<Value>(&json_bytes) {
        // Check for JSON-RPC error in response body (extract before mutable rewrite)
        let rpc_error =
            jsonrpc::extract_error_code(&json_body).map(|(code, msg)| (code, msg.to_string()));

        rewrite_response(method_str, &mut json_body, &config);
        let rewritten = serde_json::to_vec(&json_body).unwrap_or(json_bytes);
        let body = if is_sse {
            wrap_as_sse(&rewritten)
        } else {
            rewritten
        };
        let note = if is_sse { "rewritten+sse" } else { "rewritten" };
        let mut entry = LogEntry::new("POST", path, status, note)
            .mcp_method(method_str)
            .maybe_detail(call_detail.as_deref())
            .maybe_session_id(log_session_id.as_deref())
            .upstream(&upstream_url)
            .req_size(raw_body_len)
            .size(body.len())
            .upstream_duration(upstream_ms)
            .duration(start);
        if let Some((code, ref msg)) = rpc_error {
            entry = entry.jsonrpc_error(code, msg);
        }
        state.logger.emit(entry);

        // Emit structured event
        let evt_status = if rpc_error.is_some() {
            EventStatus::Error
        } else {
            EventStatus::Ok
        };
        let mut evt = McprEvent::new(event_type_for(&mcp_method))
            .method(method_str)
            .upstream(&upstream_url)
            .latency(start.elapsed().as_millis() as u64)
            .upstream_ms(upstream_ms)
            .status(evt_status)
            .request_size(raw_body_len as u64)
            .response_size(body.len() as u64);
        if let Some((_, ref msg)) = rpc_error {
            evt = evt.error_detail(msg);
        }
        if let Some(ref sid) = log_session_id {
            evt = evt.session(sid);
            if let Some(info) = state.sessions.get(sid).await.and_then(|s| s.client_info) {
                evt = evt.client_name(&info.name);
                if let Some(ref v) = info.version {
                    evt = evt.client_version(v);
                }
            }
        }
        if let Some(ref d) = call_detail
            && mcp_method == McpMethod::ToolsCall
        {
            evt = evt.tool(d);
        }
        state.events.emit(evt);

        // Record request in store.
        if let Some(ref store) = state.store {
            let store_status = if rpc_error.is_some() {
                RequestStatus::Error
            } else {
                RequestStatus::Ok
            };
            let tool = if mcp_method == McpMethod::ToolsCall {
                call_detail.clone()
            } else {
                None
            };
            store.record(StoreEvent::Request(RequestEvent {
                request_id: uuid::Uuid::new_v4().to_string(),
                ts: chrono::Utc::now().timestamp_millis(),
                proxy: state.proxy_name.clone(),
                session_id: log_session_id.clone(),
                method: method_str.to_string(),
                tool,
                latency_ms: start.elapsed().as_millis() as i64,
                status: store_status,
                error_code: rpc_error.as_ref().map(|(code, _)| code.to_string()),
                error_msg: rpc_error.map(|(_, msg)| msg.chars().take(512).collect()),
                bytes_in: Some(raw_body_len as i64),
                bytes_out: Some(body.len() as i64),
            }));
        }

        build_response(status, &resp_headers, Body::from(body))
    } else {
        state.logger.emit(
            LogEntry::new("POST", path, status, "passthrough")
                .mcp_method(method_str)
                .maybe_detail(call_detail.as_deref())
                .maybe_session_id(log_session_id.as_deref())
                .upstream(&upstream_url)
                .req_size(raw_body_len)
                .size(resp_bytes.len())
                .upstream_duration(upstream_ms)
                .duration(start),
        );
        state.events.emit(
            McprEvent::new(EventType::Request)
                .method(method_str)
                .upstream(&upstream_url)
                .latency(start.elapsed().as_millis() as u64)
                .upstream_ms(upstream_ms)
                .request_size(raw_body_len as u64)
                .response_size(resp_bytes.len() as u64),
        );

        // Record passthrough request in store.
        if let Some(ref store) = state.store {
            store.record(StoreEvent::Request(RequestEvent {
                request_id: uuid::Uuid::new_v4().to_string(),
                ts: chrono::Utc::now().timestamp_millis(),
                proxy: state.proxy_name.clone(),
                session_id: log_session_id.clone(),
                method: method_str.to_string(),
                tool: None,
                latency_ms: start.elapsed().as_millis() as i64,
                status: RequestStatus::Ok,
                error_code: None,
                error_msg: None,
                bytes_in: Some(raw_body_len as i64),
                bytes_out: Some(resp_bytes.len() as i64),
            }));
        }

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
        true, // SSE streaming — no request timeout
    )
    .await
    {
        Ok(resp) => {
            let upstream_ms = upstream_start.elapsed().as_millis() as u64;
            let status = resp.status().as_u16();
            let resp_headers = resp.headers().clone();
            state.logger.emit(
                LogEntry::new("GET", path, status, "sse")
                    .upstream(&upstream_url)
                    .upstream_duration(upstream_ms)
                    .duration(start),
            );
            build_response(
                status,
                &resp_headers,
                Body::from_stream(resp.bytes_stream()),
            )
        }
        Err(e) => {
            let upstream_ms = upstream_start.elapsed().as_millis() as u64;
            state.logger.emit(
                LogEntry::new("GET", path, 502, "upstream error")
                    .upstream(&upstream_url)
                    .upstream_duration(upstream_ms)
                    .duration(start),
            );
            state.events.emit(
                McprEvent::new(EventType::Request)
                    .method("SSE")
                    .upstream(&upstream_url)
                    .latency(start.elapsed().as_millis() as u64)
                    .upstream_ms(upstream_ms)
                    .status(EventStatus::Error)
                    .error_detail(format!("{e}")),
            );
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

    // Forward to upstream to get the metadata
    let upstream_url = state.mcp_upstream.trim_end_matches('/').to_string();
    let upstream_start = Instant::now();
    let upstream_resp =
        forward_request(state, &upstream_url, Method::POST, headers, raw_body, false)
            .await
            .ok()?;

    let upstream_bytes = read_body_capped(upstream_resp, state.max_response_body)
        .await
        .ok()?;
    let upstream_ms = upstream_start.elapsed().as_millis() as u64;
    let json_bytes =
        extract_json_from_sse(&upstream_bytes).unwrap_or_else(|| upstream_bytes.to_vec());
    let mut json_body: Value = serde_json::from_slice(&json_bytes).ok()?;

    // Replace the HTML text with our local version
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
    state.logger.emit(
        LogEntry::new("POST", "/*", 200, "intercepted")
            .mcp_method(jsonrpc::RESOURCES_READ)
            .req_size(raw_body.len())
            .size(body.len())
            .upstream_duration(upstream_ms)
            .duration(start),
    );
    state.events.emit(
        McprEvent::new(EventType::WidgetServe)
            .method(jsonrpc::RESOURCES_READ)
            .latency(start.elapsed().as_millis() as u64)
            .upstream_ms(upstream_ms)
            .request_size(raw_body.len() as u64)
            .response_size(body.len() as u64),
    );
    let mut resp_headers = HeaderMap::new();
    resp_headers.insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
    Some(build_response(200, &resp_headers, Body::from(body)))
}
