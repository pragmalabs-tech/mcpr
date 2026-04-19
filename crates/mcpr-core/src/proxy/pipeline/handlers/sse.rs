//! `stream_sse` — GET /mcp. Opens a long-lived SSE stream from
//! upstream and streams bytes straight to the client. No buffering,
//! no middleware chain.

use std::time::Instant;

use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, Method};
use axum::response::Response;

use crate::proxy::ProxyState;
use crate::proxy::forwarding::build_response;
use crate::proxy::pipeline::context::RequestContext;
use crate::proxy::pipeline::emit::{ResponseSummary, emit_request_event};

use super::{emit_upstream_error, forward_or_502};

pub async fn stream_sse(
    state: &ProxyState,
    ctx: &mut RequestContext,
    headers: &HeaderMap,
) -> Response {
    // GET SSE doesn't carry a JSON-RPC method; tag accordingly and
    // drop the request-side session id for parity with legacy behavior.
    ctx.mcp_method_str = Some("SSE".to_string());
    ctx.session_id = None;

    let upstream_url = state.mcp_upstream.trim_end_matches('/').to_string();
    let upstream_start = Instant::now();
    let resp = match forward_or_502(
        &state.upstream,
        &upstream_url,
        Method::GET,
        headers,
        &Bytes::new(),
        true,
    )
    .await
    {
        Ok(r) => r,
        Err(e) => return emit_upstream_error(state, ctx, upstream_start, e),
    };

    let status = resp.status().as_u16();
    let upstream_headers = resp.headers().clone();
    let upstream_us = upstream_start.elapsed().as_micros() as u64;

    ctx.tags.push("sse");
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
