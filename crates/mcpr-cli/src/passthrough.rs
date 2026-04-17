use std::time::Instant;

use axum::{
    body::{Body, Bytes},
    http::{HeaderMap, Method, StatusCode, header},
    response::{IntoResponse, Response},
};

use crate::AppState;
use crate::proxy::forward_request;
use mcpr_core::event::{ProxyEvent, RequestEvent};
use mcpr_core::proxy::forwarding::{build_response, read_body_capped};
use mcpr_core::proxy::sse::split_upstream;

/// Forward a request to upstream and return the response, rewriting upstream URLs in JSON bodies.
pub async fn forward_and_passthrough(
    state: &AppState,
    url: &str,
    method: Method,
    log_path: &str,
    headers: &HeaderMap,
    body: &Bytes,
    start: Instant,
) -> Response {
    let is_streaming = headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .map(|a| a.contains("text/event-stream"))
        .unwrap_or(false);
    let upstream_start = Instant::now();
    match forward_request(state, url, method.clone(), headers, body, is_streaming).await {
        Ok(resp) => {
            let status = resp.status().as_u16();
            let resp_headers = resp.headers().clone();
            let bytes = match read_body_capped(resp, state.max_response_body).await {
                Ok(b) => b,
                Err(err_resp) => return err_resp,
            };
            let upstream_us = upstream_start.elapsed().as_micros() as u64;

            // Rewrite upstream base URL → proxy URL in JSON responses
            let is_json = resp_headers
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .map(|ct| ct.contains("json"))
                .unwrap_or(false);

            let (response_body, note) = if is_json {
                let config = state.rewrite_config.read().await;
                let (upstream_base, _) = split_upstream(&config.mcp_upstream);
                let body_str = String::from_utf8_lossy(&bytes);
                let rewritten = body_str
                    .replace(
                        upstream_base.trim_end_matches('/'),
                        config.proxy_url.trim_end_matches('/'),
                    )
                    .into_bytes();
                (rewritten, "rewritten")
            } else {
                (bytes.to_vec(), "passthrough")
            };

            state
                .event_bus
                .emit(ProxyEvent::Request(Box::new(RequestEvent {
                    id: uuid::Uuid::new_v4().to_string(),
                    ts: chrono::Utc::now().timestamp_millis(),
                    proxy: state.proxy_name.clone(),
                    session_id: None,
                    method: method.to_string(),
                    path: log_path.to_string(),
                    mcp_method: None,
                    tool: None,
                    status,
                    latency_us: start.elapsed().as_micros() as u64,
                    upstream_us: Some(upstream_us),
                    request_size: Some(body.len() as u64),
                    response_size: Some(response_body.len() as u64),
                    error_code: None,
                    error_msg: None,
                    client_name: None,
                    client_version: None,
                    note: note.to_string(),
                })));

            build_response(status, &resp_headers, Body::from(response_body))
        }
        Err(e) => {
            let upstream_us = upstream_start.elapsed().as_micros() as u64;
            state
                .event_bus
                .emit(ProxyEvent::Request(Box::new(RequestEvent {
                    id: uuid::Uuid::new_v4().to_string(),
                    ts: chrono::Utc::now().timestamp_millis(),
                    proxy: state.proxy_name.clone(),
                    session_id: None,
                    method: method.to_string(),
                    path: log_path.to_string(),
                    mcp_method: None,
                    tool: None,
                    status: 502,
                    latency_us: start.elapsed().as_micros() as u64,
                    upstream_us: Some(upstream_us),
                    request_size: Some(body.len() as u64),
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
