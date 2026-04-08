use std::sync::Arc;
use std::time::Duration;

use axum::{
    body::{Body, Bytes},
    http::{HeaderMap, Method, StatusCode, header},
    response::{IntoResponse, Response},
};
use futures_util::StreamExt;
use tokio::sync::Semaphore;

/// Shared upstream connection config for forwarding requests.
#[derive(Clone)]
pub struct UpstreamClient {
    pub http_client: reqwest::Client,
    pub semaphore: Arc<Semaphore>,
    pub request_timeout: Duration,
}

/// Read a response body with a size cap. Returns 502 if the upstream response exceeds `max_bytes`.
pub async fn read_body_capped(
    resp: reqwest::Response,
    max_bytes: usize,
) -> Result<Bytes, Response> {
    if let Some(len) = resp.content_length()
        && len as usize > max_bytes
    {
        return Err((StatusCode::BAD_GATEWAY, "upstream response too large").into_response());
    }

    let mut body =
        Vec::with_capacity(resp.content_length().unwrap_or(0).min(max_bytes as u64) as usize);
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| {
            (StatusCode::BAD_GATEWAY, format!("upstream read error: {e}")).into_response()
        })?;
        if body.len() + chunk.len() > max_bytes {
            return Err((StatusCode::BAD_GATEWAY, "upstream response too large").into_response());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(Bytes::from(body))
}

/// Send a request to the upstream server, forwarding relevant headers.
/// When `is_streaming` is false, applies the configured request timeout.
pub async fn forward_request(
    upstream: &UpstreamClient,
    url: &str,
    method: Method,
    headers: &HeaderMap,
    body: &Bytes,
    is_streaming: bool,
) -> Result<reqwest::Response, reqwest::Error> {
    let _permit = upstream
        .semaphore
        .acquire()
        .await
        .expect("upstream semaphore closed");

    let mut req = upstream.http_client.request(method, url);

    if !is_streaming {
        req = req.timeout(upstream.request_timeout);
    }

    for key in [header::AUTHORIZATION, header::CONTENT_TYPE, header::ACCEPT] {
        if let Some(val) = headers.get(&key) {
            req = req.header(key.as_str(), val.as_bytes());
        }
    }

    if let Some(session_id) = headers.get("mcp-session-id") {
        req = req.header("mcp-session-id", session_id.as_bytes());
    }

    if let Some(last_event) = headers.get("last-event-id") {
        req = req.header("last-event-id", last_event.as_bytes());
    }

    if !body.is_empty() {
        req = req.body(body.clone());
    }

    req.send().await
}

/// Build an axum Response from status, headers, and body.
pub fn build_response(status: u16, upstream_headers: &HeaderMap, body: Body) -> Response {
    let status_code = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut builder = Response::builder().status(status_code);

    for key in [header::CONTENT_TYPE, header::CACHE_CONTROL] {
        if let Some(val) = upstream_headers.get(&key) {
            builder = builder.header(key.as_str(), val);
        }
    }

    if let Some(val) = upstream_headers.get("mcp-session-id") {
        builder = builder.header("mcp-session-id", val);
    }

    if let Some(val) = upstream_headers.get(header::WWW_AUTHENTICATE) {
        builder = builder.header(header::WWW_AUTHENTICATE, val);
    }

    builder.body(body).unwrap_or_else(|_| {
        Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .body(Body::from("Failed to build response"))
            .unwrap()
    })
}
