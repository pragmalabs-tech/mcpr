//! Stage-6 glue — the single entrypoint every HTTP request goes through.
//! Parses, routes, runs request middleware, forwards (or serves locally), runs
//! response middleware, emits, builds the final [`axum::Response`].

use std::sync::Arc;
use std::time::Instant;

use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};

use crate::protocol::session::SessionStore;
use crate::proxy::forwarding::{build_response, forward_request, read_body_capped};
use crate::proxy::sse::split_upstream;

use crate::proxy::pipeline::context::{RequestContext, ResponseContext};
use crate::proxy::pipeline::emit::{ResponseSummary, emit_request_event};
use crate::proxy::pipeline::middleware::{
    DeleteSessionEndMiddleware, McpHealthMiddleware, RequestMiddleware, ResponseMiddleware,
    SchemaIngestMiddleware, SessionStartMiddleware, SessionTouchMiddleware, SseUnwrapMiddleware,
    SseWrapMiddleware, StaleMarkMiddleware, UpstreamUrlMapMiddleware, UrlRewriteMiddleware,
    WidgetOverlayMiddleware,
};
use crate::proxy::pipeline::parser::build_request_context;
use crate::proxy::pipeline::route::{RouteKind, route};
use crate::proxy::proxy_state::ProxyState;
use crate::proxy::widgets::{list_widgets, serve_widget_asset, serve_widget_html};

/// Run the full proxy pipeline on one HTTP request.
pub async fn run(
    proxy: Arc<ProxyState>,
    method: Method,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> Response {
    let start = Instant::now();
    let path = uri.path();
    let has_widgets = proxy.widget_source.is_some();

    // ① Parse
    let mut ctx = build_request_context(method.clone(), path, &headers, &body, start);

    // ③ Request middleware chain
    if let Some(resp) = SessionTouchMiddleware.on_request(&proxy, &mut ctx).await {
        return resp;
    }
    if let Some(resp) = DeleteSessionEndMiddleware
        .on_request(&proxy, &mut ctx)
        .await
    {
        return resp;
    }

    // ② Route
    // ④ Handler dispatch  +  ⑤ Response middleware  +  ⑥ Emit
    match route(&ctx, &headers, has_widgets) {
        RouteKind::WidgetHtml(name) => serve_widget_html(&proxy, &name).await,
        RouteKind::WidgetList => list_widgets(&proxy).await,
        RouteKind::WidgetAsset => serve_widget_asset(&proxy, path).await,
        RouteKind::McpPost => mcp_post(&proxy, &mut ctx, &headers, &body).await,
        RouteKind::McpSse => mcp_sse(&proxy, &mut ctx, &headers).await,
        RouteKind::Passthrough => {
            let (base, _) = split_upstream(&proxy.mcp_upstream);
            let upstream_url = format!("{}{}", base.trim_end_matches('/'), path);
            passthrough(&proxy, &mut ctx, &upstream_url, &headers, &body).await
        }
    }
}

// ── Per-route handlers + middleware chain ──────────────────────────────────────────

async fn mcp_post(
    proxy: &ProxyState,
    ctx: &mut RequestContext,
    headers: &HeaderMap,
    body: &Bytes,
) -> Response {
    let upstream_url = proxy.mcp_upstream.trim_end_matches('/').to_string();
    let upstream_start = Instant::now();
    let resp = match forward_request(
        &proxy.upstream,
        &upstream_url,
        Method::POST,
        headers,
        body,
        false,
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            let upstream_us = upstream_start.elapsed().as_micros() as u64;
            populate_client_info(proxy, ctx).await;
            ctx.tags.push("upstream error");
            emit_request_event(
                proxy,
                ctx,
                &ResponseSummary {
                    status: 502,
                    response_size: None,
                    upstream_us: Some(upstream_us),
                    error_code: None,
                    error_msg: None,
                },
            );
            return (StatusCode::BAD_GATEWAY, format!("Upstream error: {e}")).into_response();
        }
    };

    let status = resp.status().as_u16();
    let resp_headers = resp.headers().clone();

    // Track session id from upstream response — overwrites the request-side
    // id so the response middleware chain sees the authoritative one.
    if let Some(resp_sid) = resp_headers
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
    {
        ctx.session_id = Some(resp_sid.to_string());
    }

    let resp_bytes = match read_body_capped(resp, proxy.max_response_body).await {
        Ok(b) => b,
        Err(err_resp) => return err_resp,
    };
    let upstream_us = upstream_start.elapsed().as_micros() as u64;

    let mut resp_ctx = ResponseContext::new(
        status,
        resp_headers.clone(),
        resp_bytes.to_vec(),
        Some(upstream_us),
    );

    // ⑤ Response middleware chain — order matters.
    SseUnwrapMiddleware
        .on_response(proxy, ctx, &mut resp_ctx)
        .await;
    SchemaIngestMiddleware
        .on_response(proxy, ctx, &mut resp_ctx)
        .await;
    StaleMarkMiddleware
        .on_response(proxy, ctx, &mut resp_ctx)
        .await;
    UrlRewriteMiddleware
        .on_response(proxy, ctx, &mut resp_ctx)
        .await;
    WidgetOverlayMiddleware
        .on_response(proxy, ctx, &mut resp_ctx)
        .await;
    McpHealthMiddleware
        .on_response(proxy, ctx, &mut resp_ctx)
        .await;
    SessionStartMiddleware
        .on_response(proxy, ctx, &mut resp_ctx)
        .await;
    SseWrapMiddleware
        .on_response(proxy, ctx, &mut resp_ctx)
        .await;

    populate_client_info(proxy, ctx).await;

    // ⑥ Emit — compose tags from response shape
    if resp_ctx.json.is_some() {
        ctx.tags.push("rewritten");
        if resp_ctx.was_sse {
            ctx.tags.push("sse");
        }
    } else {
        ctx.tags.push("passthrough");
    }
    let mut summary = ResponseSummary {
        status: resp_ctx.status,
        response_size: Some(resp_ctx.body.len() as u64),
        upstream_us: resp_ctx.upstream_us,
        error_code: None,
        error_msg: None,
    };
    if let Some((code, msg)) = resp_ctx.rpc_error.clone() {
        summary = summary.with_rpc_error(code, msg);
    }
    emit_request_event(proxy, ctx, &summary);

    build_response(
        resp_ctx.status,
        &resp_ctx.headers,
        Body::from(resp_ctx.body),
    )
}

async fn mcp_sse(proxy: &ProxyState, ctx: &mut RequestContext, headers: &HeaderMap) -> Response {
    // SSE GETs don't carry a JSON-RPC method; tag the event with "SSE" and
    // drop the request-side session id for parity with today's behavior.
    ctx.mcp_method_str = Some("SSE".to_string());
    ctx.session_id = None;

    let upstream_url = proxy.mcp_upstream.trim_end_matches('/').to_string();
    let upstream_start = Instant::now();
    match forward_request(
        &proxy.upstream,
        &upstream_url,
        Method::GET,
        headers,
        &Bytes::new(),
        true,
    )
    .await
    {
        Ok(resp) => {
            let status = resp.status().as_u16();
            let resp_headers = resp.headers().clone();
            let upstream_us = upstream_start.elapsed().as_micros() as u64;
            ctx.tags.push("sse");
            emit_request_event(
                proxy,
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
                &resp_headers,
                Body::from_stream(resp.bytes_stream()),
            )
        }
        Err(e) => {
            let upstream_us = upstream_start.elapsed().as_micros() as u64;
            ctx.tags.push("upstream error");
            emit_request_event(
                proxy,
                ctx,
                &ResponseSummary {
                    status: 502,
                    response_size: None,
                    upstream_us: Some(upstream_us),
                    error_code: None,
                    error_msg: Some(format!("{e}")),
                },
            );
            (StatusCode::BAD_GATEWAY, format!("Upstream error: {e}")).into_response()
        }
    }
}

async fn passthrough(
    proxy: &ProxyState,
    ctx: &mut RequestContext,
    upstream_url: &str,
    headers: &HeaderMap,
    body: &Bytes,
) -> Response {
    // Preserve today's behavior: passthrough doesn't log session or client.
    ctx.session_id = None;

    let upstream_start = Instant::now();
    let resp = forward_request(
        &proxy.upstream,
        upstream_url,
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
            let bytes = match read_body_capped(resp, proxy.max_response_body).await {
                Ok(b) => b,
                Err(err_resp) => return err_resp,
            };
            let upstream_us = upstream_start.elapsed().as_micros() as u64;

            let mut resp_ctx =
                ResponseContext::new(status, resp_headers, bytes.to_vec(), Some(upstream_us));

            UpstreamUrlMapMiddleware
                .on_response(proxy, ctx, &mut resp_ctx)
                .await;

            let is_json = resp_ctx
                .headers
                .get(axum::http::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .map(|ct| ct.contains("json"))
                .unwrap_or(false);
            ctx.tags
                .push(if is_json { "rewritten" } else { "passthrough" });

            emit_request_event(
                proxy,
                ctx,
                &ResponseSummary {
                    status: resp_ctx.status,
                    response_size: Some(resp_ctx.body.len() as u64),
                    upstream_us: resp_ctx.upstream_us,
                    error_code: None,
                    error_msg: None,
                },
            );

            build_response(
                resp_ctx.status,
                &resp_ctx.headers,
                Body::from(resp_ctx.body),
            )
        }
        Err(e) => {
            let upstream_us = upstream_start.elapsed().as_micros() as u64;
            ctx.tags.push("upstream error");
            emit_request_event(
                proxy,
                ctx,
                &ResponseSummary {
                    status: 502,
                    response_size: None,
                    upstream_us: Some(upstream_us),
                    error_code: None,
                    error_msg: Some(format!("{e}")),
                },
            );
            (StatusCode::BAD_GATEWAY, format!("Upstream error: {e}")).into_response()
        }
    }
}

/// Look up client name/version from the session store (if a session id is
/// known) and stash them on the context so `emit_request_event` picks them up.
async fn populate_client_info(proxy: &ProxyState, ctx: &mut RequestContext) {
    if let Some(ref sid) = ctx.session_id
        && let Some(info) = proxy.sessions.get(sid).await
        && let Some(ci) = info.client_info
    {
        ctx.client_name = Some(ci.name);
        ctx.client_version = ci.version;
    }
}
