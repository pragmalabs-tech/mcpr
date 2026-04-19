//! `passthrough` — non-MCP routes (OAuth endpoints, health probes,
//! etc.). Buffers the response only if content-type is JSON so the
//! upstream-URL substitution step can run. Binary / HTML / other
//! content streams through.

use std::time::Instant;

use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, header};
use axum::response::Response;

use crate::proxy::ProxyState;
use crate::proxy::forwarding::{build_response, read_body_capped};
use crate::proxy::pipeline::context::RequestContext;
use crate::proxy::pipeline::emit::{ResponseSummary, emit_request_event};
use crate::proxy::pipeline::steps::url_map;
use crate::proxy::sse::split_upstream;

use super::{emit_upstream_error, forward_or_502};

pub async fn passthrough(
    state: &ProxyState,
    ctx: &mut RequestContext,
    path: &str,
    headers: &HeaderMap,
    body: &Bytes,
) -> Response {
    // Parity with legacy behavior — passthrough doesn't log session/client.
    ctx.session_id = None;

    let (base, _) = split_upstream(&state.mcp_upstream);
    let upstream_url = format!("{}{}", base.trim_end_matches('/'), path);

    let upstream_start = Instant::now();
    let resp = match forward_or_502(
        &state.upstream,
        &upstream_url,
        ctx.http_method.clone(),
        headers,
        body,
        ctx.wants_sse,
    )
    .await
    {
        Ok(r) => r,
        Err(e) => return emit_upstream_error(state, ctx, upstream_start, e),
    };

    let status = resp.status().as_u16();
    let upstream_headers = resp.headers().clone();
    let is_json = upstream_headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|ct| ct.contains("json"))
        .unwrap_or(false);

    if is_json {
        // Buffer so we can do URL substitution.
        let bytes = match read_body_capped(resp, state.max_response_body).await {
            Ok(b) => b,
            Err(err_resp) => return err_resp,
        };
        let upstream_us = upstream_start.elapsed().as_micros() as u64;
        let mut timer = super::StageTimer::new();
        let rewritten = url_map::rewrite_passthrough_urls(&state.rewrite_config, bytes);
        timer.mark(super::Stage::UrlMap);
        ctx.tags.push("rewritten");
        emit_request_event(
            state,
            ctx,
            &ResponseSummary {
                status,
                response_size: Some(rewritten.len() as u64),
                upstream_us: Some(upstream_us),
                error_code: None,
                error_msg: None,
                stage_timings: timer.finish(),
            },
        );
        build_response(status, &upstream_headers, Body::from(rewritten.to_vec()))
    } else {
        // Non-JSON — stream through unchanged.
        let upstream_us = upstream_start.elapsed().as_micros() as u64;
        ctx.tags.push("passthrough");
        emit_request_event(
            state,
            ctx,
            &ResponseSummary {
                status,
                response_size: None,
                upstream_us: Some(upstream_us),
                error_code: None,
                error_msg: None,
                stage_timings: None,
            },
        );
        build_response(
            status,
            &upstream_headers,
            Body::from_stream(resp.bytes_stream()),
        )
    }
}
