//! `forward_and_stream` — for MCP POST methods whose responses never
//! need mutation (`initialize`, `ping`, `notifications/*`, prompts,
//! completion, logging). Streams the upstream body straight through
//! via `axum::body::Body::from_stream` — no buffering, no parse, no
//! reserialize. This is the majority case.

use std::time::Instant;

use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, Method};
use axum::response::Response;

use crate::protocol::McpMethod;
use crate::proxy::ProxyState;
use crate::proxy::forwarding::build_response;
use crate::proxy::pipeline::context::RequestContext;
use crate::proxy::pipeline::emit::{ResponseSummary, emit_request_event};
use crate::proxy::pipeline::steps::{health, session};

use super::{capture_session_id, emit_upstream_error, forward_or_502, populate_client_info};

/// Forward the request to upstream and stream the response body back
/// unchanged. Runs only method-aware side effects (session start on
/// successful initialize, health update) — no body parse.
pub async fn forward_and_stream(
    state: &ProxyState,
    ctx: &mut RequestContext,
    method: &McpMethod,
    headers: &HeaderMap,
    body: &Bytes,
) -> Response {
    let upstream_url = state.mcp_upstream.trim_end_matches('/').to_string();
    let upstream_start = Instant::now();
    let resp = match forward_or_502(
        &state.upstream,
        &upstream_url,
        Method::POST,
        headers,
        body,
        false,
    )
    .await
    {
        Ok(r) => r,
        Err(e) => return emit_upstream_error(state, ctx, upstream_start, e),
    };

    let status = resp.status().as_u16();
    let upstream_headers = resp.headers().clone();
    let upstream_us = upstream_start.elapsed().as_micros() as u64;

    capture_session_id(ctx, &upstream_headers);

    // Method-aware side effects — reading headers/status/method only.
    health::track_post_response(state, method, status);
    session::maybe_record_start(state, ctx, method, status).await;
    populate_client_info(state, ctx).await;

    // Tag for observability: matches the legacy `"rewritten"` semantics
    // when JSON was parsed. For the streamed path we never parse, so we
    // don't have rpc_error info — emit with just the transport-level
    // status and let downstream tools decode errors from the body if
    // they care.
    ctx.tags.push("streamed");

    emit_request_event(
        state,
        ctx,
        &ResponseSummary {
            status,
            response_size: None,
            upstream_us: Some(upstream_us),
            error_code: None,
            error_msg: None,
        },
    );

    build_response(
        status,
        &upstream_headers,
        Body::from_stream(resp.bytes_stream()),
    )
}
