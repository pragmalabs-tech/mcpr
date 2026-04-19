//! Concrete per-shape request handlers. Each function handles one
//! [`super::route::RequestKind`] variant end-to-end: forward, run
//! steps, emit, return `axum::Response`.
//!
//! The old `ResponseMiddleware` trait is gone — transforms are called
//! explicitly as functions in `crate::proxy::pipeline::steps::*`.

pub mod buffered;
pub mod header_phase;
pub mod passthrough;
pub mod sse;
pub mod streamed;

use std::time::Instant;

use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::{IntoResponse, Response};

use crate::protocol::session::SessionStore;
use crate::proxy::ProxyState;
use crate::proxy::forwarding::{UpstreamClient, forward_request};
use crate::proxy::pipeline::context::RequestContext;
use crate::proxy::pipeline::emit::{ResponseSummary, emit_request_event};

// Per-stage instrumentation moved to `crate::timing`. It's gated on
// the `MCPR_STAGE_TIMING` env var, so hot-path overhead is ~1 ns when
// disabled (the default). See `crate::timing::StageTimer`.
pub(super) use crate::timing::{Stage, StageTimer};

/// Forward to upstream, returning either the `reqwest::Response` or a
/// pre-built 502 `Response` (which the caller should return directly).
/// Consolidates the upstream-error → 502 block that was copy-pasted
/// across three handlers.
pub(super) async fn forward_or_502(
    upstream: &UpstreamClient,
    url: &str,
    method: Method,
    headers: &HeaderMap,
    body: &axum::body::Bytes,
    is_streaming: bool,
) -> Result<reqwest::Response, ForwardError> {
    forward_request(upstream, url, method, headers, body, is_streaming)
        .await
        .map_err(|e| ForwardError {
            err_string: format!("{e}"),
        })
}

pub(super) struct ForwardError {
    pub err_string: String,
}

/// Emit a 502 event + return a bare 502 response. Used by every
/// handler when upstream errors out.
pub(super) fn emit_upstream_error(
    state: &ProxyState,
    ctx: &mut RequestContext,
    upstream_start: Instant,
    err: ForwardError,
) -> Response {
    let upstream_us = upstream_start.elapsed().as_micros() as u64;
    ctx.tags.push("upstream error");
    emit_request_event(
        state,
        ctx,
        &ResponseSummary {
            status: 502,
            response_size: None,
            upstream_us: Some(upstream_us),
            error_code: None,
            error_msg: Some(err.err_string.clone()),
            stage_timings: None,
        },
    );
    (
        StatusCode::BAD_GATEWAY,
        format!("Upstream error: {}", err.err_string),
    )
        .into_response()
}

/// Look up client name/version from the session store (if a session id
/// is known) and stash them on the context so `emit_request_event`
/// picks them up.
pub(super) async fn populate_client_info(state: &ProxyState, ctx: &mut RequestContext) {
    if let Some(ref sid) = ctx.session_id
        && let Some(info) = state.sessions.get(sid).await
        && let Some(ci) = info.client_info
    {
        ctx.client_name = Some(ci.name);
        ctx.client_version = ci.version;
    }
}

/// Capture `mcp-session-id` from upstream response headers into the
/// request context. Both buffered and streamed post handlers do this
/// identically.
pub(super) fn capture_session_id(ctx: &mut RequestContext, upstream_headers: &HeaderMap) {
    if let Some(sid) = upstream_headers
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
    {
        ctx.session_id = Some(sid.to_string());
    }
}
