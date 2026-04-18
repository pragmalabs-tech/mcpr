use std::time::Instant;

use axum::{
    body::{Body, Bytes},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};

use crate::pipeline::{
    RequestContext, ResponseContext, ResponseMw, ResponseSummary, UpstreamUrlMapMw,
    emit_request_event,
};
use crate::proxy::forward_request;
use crate::state::ProxyState;
use mcpr_core::proxy::forwarding::{build_response, read_body_capped};

/// Forward a request to upstream and return the response, rewriting upstream
/// URLs in JSON bodies via [`UpstreamUrlMapMw`].
pub async fn forward_and_passthrough(
    state: &ProxyState,
    ctx: &mut RequestContext,
    url: &str,
    headers: &HeaderMap,
    body: &Bytes,
) -> Response {
    // Preserve today's behavior: passthrough doesn't log session or client.
    ctx.session_id = None;

    let upstream_start = Instant::now();
    let resp = forward_request(
        state,
        url,
        ctx.http_method.clone(),
        headers,
        body,
        ctx.wants_sse,
    )
    .await;

    match resp {
        Ok(resp) => {
            let status = resp.status().as_u16();
            let resp_headers = resp.headers().clone();
            let bytes = match read_body_capped(resp, state.max_response_body).await {
                Ok(b) => b,
                Err(err_resp) => return err_resp,
            };
            let upstream_us = upstream_start.elapsed().as_micros() as u64;

            let mut resp_ctx =
                ResponseContext::new(status, resp_headers, bytes.to_vec(), Some(upstream_us));

            UpstreamUrlMapMw
                .on_response(state, ctx, &mut resp_ctx)
                .await;

            // Today's behavior: note="rewritten" if response was JSON,
            // "passthrough" otherwise. The mw is a no-op for non-JSON so we
            // can infer the path from content-type on `resp_ctx.headers`.
            let note = if resp_ctx
                .headers
                .get(axum::http::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .map(|ct| ct.contains("json"))
                .unwrap_or(false)
            {
                "rewritten"
            } else {
                "passthrough"
            };

            emit_request_event(
                state,
                ctx,
                &ResponseSummary {
                    status: resp_ctx.status,
                    response_size: Some(resp_ctx.body.len() as u64),
                    upstream_us: resp_ctx.upstream_us,
                    error_code: None,
                    error_msg: None,
                },
                note,
            );

            build_response(
                resp_ctx.status,
                &resp_ctx.headers,
                Body::from(resp_ctx.body),
            )
        }
        Err(e) => {
            let upstream_us = upstream_start.elapsed().as_micros() as u64;
            emit_request_event(
                state,
                ctx,
                &ResponseSummary {
                    status: 502,
                    response_size: None,
                    upstream_us: Some(upstream_us),
                    error_code: None,
                    error_msg: Some(format!("{e}")),
                },
                "upstream error",
            );
            (StatusCode::BAD_GATEWAY, format!("Upstream error: {e}")).into_response()
        }
    }
}
