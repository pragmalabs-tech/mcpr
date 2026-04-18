use std::time::Instant;

use axum::{
    body::{Body, Bytes},
    http::{HeaderMap, Method, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde_json::Value;

use crate::pipeline::{
    RequestContext, ResponseContext, ResponseMw, ResponseSummary, SchemaIngestMw, SseUnwrapMw,
    SseWrapMw, StaleMarkMw, UrlRewriteMw, emit_request_event, normalize_platform,
};
use crate::proxy::forward_request;
use crate::state::ProxyState;
use crate::widgets::fetch_widget_html;
use mcpr_core::event::{ProxyEvent, RequestEvent, SessionStartEvent};
use mcpr_core::protocol::session::{SessionState, SessionStore};
use mcpr_core::protocol::{self as jsonrpc, McpMethod};
use mcpr_core::proxy::forwarding::{build_response, read_body_capped};
use mcpr_core::proxy::rewrite_response;
use mcpr_core::proxy::sse::extract_json_from_sse;

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

    // Collect full body for rewriting (POST SSE is finite), capped to prevent OOM.
    let resp_bytes = match read_body_capped(resp, state.max_response_body).await {
        Ok(b) => b,
        Err(err_resp) => return err_resp,
    };
    let upstream_us = upstream_start.elapsed().as_micros() as u64;

    let mut resp_ctx = ResponseContext::new(
        status,
        resp_headers.clone(),
        resp_bytes.to_vec(),
        Some(upstream_us),
    );

    // Response middleware chain — order matters.
    SseUnwrapMw.on_response(state, ctx, &mut resp_ctx).await;
    SchemaIngestMw.on_response(state, ctx, &mut resp_ctx).await;
    StaleMarkMw.on_response(state, ctx, &mut resp_ctx).await;
    UrlRewriteMw.on_response(state, ctx, &mut resp_ctx).await;
    SseWrapMw.on_response(state, ctx, &mut resp_ctx).await;

    // Build summary + emit.
    let note = if resp_ctx.json.is_some() {
        if resp_ctx.was_sse {
            "rewritten+sse"
        } else {
            "rewritten"
        }
    } else {
        "passthrough"
    };
    let mut summary = ResponseSummary {
        status: resp_ctx.status,
        response_size: Some(resp_ctx.body.len() as u64),
        upstream_us: resp_ctx.upstream_us,
        error_code: None,
        error_msg: None,
    };
    if let Some((code, msg)) = resp_ctx.rpc_error.clone() {
        summary = summary.with_rpc_error(code, msg);
    }
    emit_request_event(state, ctx, &summary, note);

    build_response(
        resp_ctx.status,
        &resp_ctx.headers,
        Body::from(resp_ctx.body),
    )
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
