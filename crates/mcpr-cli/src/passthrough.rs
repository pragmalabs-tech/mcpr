use std::time::Instant;

use axum::{
    body::{Body, Bytes},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};

use crate::pipeline::{RequestContext, ResponseSummary, emit_request_event};
use crate::proxy::forward_request;
use crate::state::ProxyState;
use mcpr_core::proxy::forwarding::{build_response, read_body_capped};
use mcpr_core::proxy::sse::split_upstream;

/// Forward a request to upstream and return the response, rewriting upstream URLs in JSON bodies.
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

            emit_request_event(
                state,
                ctx,
                &ResponseSummary {
                    status,
                    response_size: Some(response_body.len() as u64),
                    upstream_us: Some(upstream_us),
                    error_code: None,
                    error_msg: None,
                },
                note,
            );

            build_response(status, &resp_headers, Body::from(response_body))
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
