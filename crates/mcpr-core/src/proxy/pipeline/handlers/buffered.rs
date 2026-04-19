//! `forward_and_buffer` — for MCP POST methods whose responses may
//! need mutation (`tools/*`, `resources/*`). Buffers the upstream body,
//! decodes optional SSE framing, parses JSON, runs schema/widget/rewrite
//! steps, and only reserializes if something actually changed. When the
//! body doesn't need mutation it passes through byte-for-byte.

use std::time::Instant;

use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, Method};
use axum::response::Response;
use serde_json::Value;

use crate::protocol::{self as jsonrpc, McpMethod};
use crate::proxy::ProxyState;
use crate::proxy::forwarding::{build_response, read_body_capped};
use crate::proxy::pipeline::context::RequestContext;
use crate::proxy::pipeline::emit::{ResponseSummary, emit_request_event};
use crate::proxy::pipeline::steps::{health, rewrite, schema, session, widget};
use crate::proxy::sse::{extract_json_from_sse, wrap_as_sse};

use super::{capture_session_id, emit_upstream_error, forward_or_502, populate_client_info};

pub async fn forward_and_buffer(
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

    capture_session_id(ctx, &upstream_headers);

    // Buffer the body — size-capped, returns a 502 response on overflow.
    let raw = match read_body_capped(resp, state.max_response_body).await {
        Ok(b) => b,
        Err(err_resp) => return err_resp,
    };
    let upstream_us = upstream_start.elapsed().as_micros() as u64;

    // Try SSE unwrap. If the body is text/event-stream with exactly
    // one JSON `data:` event, extract the inner bytes so middleware
    // sees the JSON. Multi-event or non-SSE bodies go through as-is.
    let (json_bytes, was_sse): (Vec<u8>, bool) = match extract_json_from_sse(&raw) {
        Some(v) => (v, true),
        None => (raw.to_vec(), false),
    };

    // Parse JSON once. Error bodies and non-JSON SSE fall through here
    // with `parsed = None` → schema/widget/rewrite all no-op.
    let mut parsed: Option<Value> = serde_json::from_slice(&json_bytes).ok();
    let rpc_error = parsed
        .as_ref()
        .and_then(|v| jsonrpc::extract_error_code(v).map(|(c, m)| (c, m.to_string())));

    let mut mutated = false;
    if let Some(json) = parsed.as_mut() {
        schema::ingest(state, ctx, json).await;
        schema::mark_stale_if_listchanged(state, json);

        if widget::maybe_overlay(state, ctx, json).await {
            mutated = true;
        }

        if let Some(method_str) = ctx.mcp_method_str.as_deref()
            && rewrite::has_markers(&json_bytes)
            && rewrite::rewrite_in_place(&state.rewrite_config, method_str, json)
        {
            mutated = true;
        }
    }

    // Method-aware side effects.
    health::track_post_response(state, method, status);
    session::maybe_record_start(state, ctx, method, status).await;
    populate_client_info(state, ctx).await;

    // Final response body — byte-pass when nothing mutated, reserialize
    // + rewrap when something did.
    let final_body: Vec<u8> = if mutated {
        match parsed.as_ref().and_then(|v| serde_json::to_vec(v).ok()) {
            Some(serialized) if was_sse => wrap_as_sse(&serialized),
            Some(serialized) => serialized,
            None => raw.to_vec(),
        }
    } else {
        raw.to_vec()
    };

    // Tag: `rewritten` when we parsed JSON (matches legacy emit tag
    // semantics, not literally "did we mutate"); `sse` if upstream was
    // SSE-framed; `passthrough` when we couldn't parse.
    if parsed.is_some() {
        ctx.tags.push("rewritten");
        if was_sse {
            ctx.tags.push("sse");
        }
    } else {
        ctx.tags.push("passthrough");
    }

    let mut summary = ResponseSummary {
        status,
        response_size: Some(final_body.len() as u64),
        upstream_us: Some(upstream_us),
        error_code: None,
        error_msg: None,
    };
    if let Some((code, msg)) = rpc_error {
        summary = summary.with_rpc_error(code, msg);
    }
    emit_request_event(state, ctx, &summary);

    build_response(status, &upstream_headers, Body::from(final_body))
}
