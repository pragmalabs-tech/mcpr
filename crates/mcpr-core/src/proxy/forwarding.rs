use std::time::Duration;

use axum::{
    body::{Body, Bytes},
    http::{HeaderMap, Method, StatusCode, header},
    response::Response,
};
use futures_util::StreamExt;

/// Shared upstream connection config for forwarding requests.
///
/// Concurrency limiting moved out of this struct in Phase 6 — a
/// `tower::limit::ConcurrencyLimitLayer` wraps the axum service at the
/// HTTP boundary. `request_timeout` stays here only for streaming paths
/// where the tower layer can't reach (tower-http timeout cancels at
/// response start, not mid-stream).
#[derive(Clone)]
pub struct UpstreamClient {
    pub http_client: reqwest::Client,
    pub request_timeout: Duration,
}

/// Reason the upstream body could not be read.
///
/// Typed so `ProxyTransport` can map it to `Response::Upstream502` with
/// a meaningful reason string — Phase 6 replacement for the old axum-
/// response return shape that couldn't carry through the response
/// middleware chain.
#[derive(Debug)]
pub enum ReadBodyError {
    /// `Content-Length` or streamed bytes exceeded `max_bytes`.
    TooLarge,
    /// Underlying reqwest stream error.
    Stream(reqwest::Error),
}

impl std::fmt::Display for ReadBodyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReadBodyError::TooLarge => write!(f, "upstream response too large"),
            ReadBodyError::Stream(e) => write!(f, "upstream read error: {e}"),
        }
    }
}

/// Read a response body with a size cap. Returns a typed error on
/// overflow or stream failure — callers produce the appropriate
/// `Response` variant (`Upstream502`) so the failure flows through the
/// response chain like any other upstream problem.
pub async fn read_body_capped(
    resp: reqwest::Response,
    max_bytes: usize,
) -> Result<Bytes, ReadBodyError> {
    if let Some(len) = resp.content_length()
        && len as usize > max_bytes
    {
        return Err(ReadBodyError::TooLarge);
    }

    let mut body =
        Vec::with_capacity(resp.content_length().unwrap_or(0).min(max_bytes as u64) as usize);
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(ReadBodyError::Stream)?;
        if body.len() + chunk.len() > max_bytes {
            return Err(ReadBodyError::TooLarge);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(Bytes::from(body))
}

/// Send a request to the upstream server, forwarding relevant headers.
///
/// For streaming calls we still apply the per-request reqwest timeout:
/// the axum-edge `TimeoutLayer` (tower-http) cancels at response start
/// and can't see mid-stream stalls. For non-streaming calls the tower
/// layer handles timeout budget end-to-end, so no reqwest timeout here.
pub async fn forward_request(
    upstream: &UpstreamClient,
    url: &str,
    method: Method,
    headers: &HeaderMap,
    body: &Bytes,
    is_streaming: bool,
) -> Result<reqwest::Response, reqwest::Error> {
    let mut req = upstream.http_client.request(method, url);

    if is_streaming {
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
