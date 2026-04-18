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

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use axum::{Router, routing::post};
    use serde_json::{Value, json};
    use tokio::sync::RwLock;

    use super::*;
    use crate::event::{EventBusHandle, EventManager, EventSink, ProxyEvent};
    use crate::protocol::schema_manager::{MemorySchemaStore, SchemaManager};
    use crate::protocol::session::MemorySessionStore;
    use crate::proxy::forwarding::UpstreamClient;
    use crate::proxy::{CspConfig, RewriteConfig, WidgetSource, new_shared_health};

    // ── Capturing sink ────────────────────────────────────────────────────

    #[derive(Clone, Default)]
    struct CapturingSink {
        events: Arc<Mutex<Vec<ProxyEvent>>>,
    }
    impl CapturingSink {
        fn snapshot(&self) -> Vec<ProxyEvent> {
            self.events.lock().unwrap().clone()
        }
    }
    impl EventSink for CapturingSink {
        fn on_event(&self, event: &ProxyEvent) {
            self.events.lock().unwrap().push(event.clone());
        }
        fn name(&self) -> &'static str {
            "capturing"
        }
    }

    // ── Harness helpers ───────────────────────────────────────────────────

    /// Spawn a mock upstream axum app on a random port and return its base URL.
    /// The URL does not include a path — callers append whichever path they
    /// configure in the upstream router (e.g. `"/mcp"`).
    async fn spawn_upstream(app: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    /// Build a [`ProxyState`] pointed at `mcp_upstream`, plus a sink that
    /// captures every emitted event. Returns the bus handle so the test can
    /// call `handle.shutdown().await` before asserting — shutdown drains the
    /// event channel, guaranteeing all `emit()` calls have been observed.
    fn build_test_proxy(
        mcp_upstream: &str,
        widget_source: Option<WidgetSource>,
    ) -> (Arc<ProxyState>, CapturingSink, EventBusHandle) {
        let sink = CapturingSink::default();
        let mut mgr = EventManager::new();
        mgr.register(Box::new(sink.clone()));
        let handle = mgr.start();
        let proxy = Arc::new(ProxyState {
            name: "test".to_string(),
            mcp_upstream: mcp_upstream.to_string(),
            upstream: UpstreamClient {
                http_client: reqwest::Client::builder()
                    .connect_timeout(Duration::from_secs(2))
                    .build()
                    .unwrap(),
                semaphore: Arc::new(tokio::sync::Semaphore::new(10)),
                request_timeout: Duration::from_secs(5),
            },
            max_request_body: 1 << 20,
            max_response_body: 1 << 20,
            rewrite_config: Arc::new(RwLock::new(RewriteConfig {
                proxy_url: "https://proxy.test".to_string(),
                proxy_domain: "proxy.test".to_string(),
                mcp_upstream: mcp_upstream.to_string(),
                csp: CspConfig::default(),
            })),
            widget_source,
            sessions: MemorySessionStore::new(),
            schema_manager: Arc::new(SchemaManager::new("test", MemorySchemaStore::new())),
            health: new_shared_health(),
            event_bus: handle.bus.clone(),
        });
        (proxy, sink, handle)
    }

    fn post_mcp(path: &str, body: Value) -> (Method, HeaderMap, Uri, Bytes) {
        let uri: Uri = format!("http://proxy.test{path}").parse().unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::CONTENT_TYPE,
            "application/json".parse().unwrap(),
        );
        let body_bytes = Bytes::from(serde_json::to_vec(&body).unwrap());
        (Method::POST, headers, uri, body_bytes)
    }

    fn only_schema_event(events: &[ProxyEvent]) -> &crate::event::SchemaVersionCreatedEvent {
        events
            .iter()
            .find_map(|e| match e {
                ProxyEvent::SchemaVersionCreated(s) => Some(s),
                _ => None,
            })
            .expect("expected a SchemaVersionCreated event")
    }

    // ── Test #1 — ordering canary ─────────────────────────────────────────
    //
    // SchemaIngestMiddleware must run BEFORE UrlRewriteMiddleware so the
    // schema store captures the upstream's original CSP, not the proxy-
    // rewritten one. If someone swaps those two in `run.rs`'s list, this
    // test fails loudly.

    #[tokio::test]
    async fn schema_ingest_sees_raw_upstream_csp_before_rewrite() {
        // Upstream returns tools/list with a widget tool whose meta declares
        // a localhost upstream in its CSP — the exact value UrlRewriteMw
        // would strip.
        let upstream_body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "tools": [{
                    "name": "search",
                    "meta": {
                        "openai/widgetCSP": {
                            "connect_domains": ["http://localhost:9000"],
                            "resource_domains": [],
                            "frame_domains": []
                        }
                    }
                }]
            }
        });
        let body_for_route = upstream_body.clone();
        let upstream = Router::new().route(
            "/mcp",
            post(move || {
                let b = body_for_route.clone();
                async move { axum::Json(b) }
            }),
        );
        let upstream_url = format!("{}/mcp", spawn_upstream(upstream).await);

        let (proxy, sink, handle) = build_test_proxy(&upstream_url, None);
        let (method, headers, uri, body) = post_mcp(
            "/mcp",
            json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
        );
        let _ = run(proxy, method, headers, uri, body).await;

        handle.shutdown().await;

        let events = sink.snapshot();
        let schema = only_schema_event(&events);
        let captured: Vec<&str> =
            schema.payload["tools"][0]["meta"]["openai/widgetCSP"]["connect_domains"]
                .as_array()
                .expect("connect_domains array")
                .iter()
                .filter_map(|v| v.as_str())
                .collect();
        assert_eq!(
            captured,
            vec!["http://localhost:9000"],
            "schema must store the UPSTREAM CSP — if rewrite ran first, this \
             would be the proxy URL and localhost would be stripped"
        );
    }

    // ── Test #2 — Initialize happy path ───────────────────────────────────

    #[tokio::test]
    async fn initialize_creates_session_and_emits_start_event() {
        let upstream_resp = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "serverInfo": {"name": "mock", "version": "0.1"}
            }
        });
        let upstream = Router::new().route(
            "/mcp",
            post(move || {
                let b = upstream_resp.clone();
                async move { ([("mcp-session-id", "sess-abc")], axum::Json(b)) }
            }),
        );
        let upstream_url = format!("{}/mcp", spawn_upstream(upstream).await);

        let (proxy, sink, handle) = build_test_proxy(&upstream_url, None);
        let (method, headers, uri, body) = post_mcp(
            "/mcp",
            json!({
                "jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": {
                    "protocolVersion": "2025-03-26",
                    "capabilities": {},
                    "clientInfo": {"name": "claude-desktop", "version": "1.2.0"}
                }
            }),
        );
        let _ = run(proxy.clone(), method, headers, uri, body).await;

        handle.shutdown().await;

        let events = sink.snapshot();
        let session_start = events
            .iter()
            .find_map(|e| match e {
                ProxyEvent::SessionStart(s) => Some(s),
                _ => None,
            })
            .expect("SessionStart event missing");
        assert_eq!(session_start.session_id, "sess-abc");
        assert_eq!(session_start.client_name.as_deref(), Some("claude-desktop"));
        assert_eq!(session_start.client_version.as_deref(), Some("1.2.0"));
        assert_eq!(session_start.client_platform.as_deref(), Some("claude"));

        let req = events
            .iter()
            .find_map(|e| match e {
                ProxyEvent::Request(r) => Some(r.as_ref()),
                _ => None,
            })
            .expect("Request event missing");
        assert_eq!(req.mcp_method.as_deref(), Some("initialize"));
        assert_eq!(req.session_id.as_deref(), Some("sess-abc"));
        assert_eq!(req.client_name.as_deref(), Some("claude-desktop"));

        assert!(matches!(
            crate::proxy::lock_health(&proxy.health).mcp_status,
            crate::proxy::ConnectionStatus::Connected
        ));
    }

    // ── Test #3 — tools/list ingests schema and emits Request event ───────

    #[tokio::test]
    async fn tools_list_emits_schema_version_and_request_event() {
        let upstream = Router::new().route(
            "/mcp",
            post(|| async {
                axum::Json(json!({
                    "jsonrpc": "2.0", "id": 1,
                    "result": {"tools": [{"name": "search"}]}
                }))
            }),
        );
        let upstream_url = format!("{}/mcp", spawn_upstream(upstream).await);

        let (proxy, sink, handle) = build_test_proxy(&upstream_url, None);
        let (method, headers, uri, body) = post_mcp(
            "/mcp",
            json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
        );
        let _ = run(proxy, method, headers, uri, body).await;

        handle.shutdown().await;

        let events = sink.snapshot();
        let schema = only_schema_event(&events);
        assert_eq!(schema.method, "tools/list");
        assert_eq!(schema.version, 1);

        let req = events
            .iter()
            .find_map(|e| match e {
                ProxyEvent::Request(r) => Some(r.as_ref()),
                _ => None,
            })
            .expect("Request event missing");
        assert_eq!(req.mcp_method.as_deref(), Some("tools/list"));
        assert_eq!(req.status, 200);
        assert_eq!(req.note, "rewritten");
    }

    // ── Test #4 — 502 when upstream is unreachable ────────────────────────

    #[tokio::test]
    async fn upstream_unreachable_returns_502_and_emits_error_event() {
        // Port 1 is reserved, no real server listens there — yields a
        // connect error fast.
        let upstream_url = "http://127.0.0.1:1/mcp";

        let (proxy, sink, handle) = build_test_proxy(upstream_url, None);
        let (method, headers, uri, body) = post_mcp(
            "/mcp",
            json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
        );
        let resp = run(proxy, method, headers, uri, body).await;

        handle.shutdown().await;

        assert_eq!(resp.status(), axum::http::StatusCode::BAD_GATEWAY);

        let events = sink.snapshot();
        let req = only_request_event(&events);
        assert_eq!(req.status, 502);
        assert_eq!(req.note, "upstream error");
    }

    // ── Test #5 — SSE GET streams from upstream ───────────────────────────

    #[tokio::test]
    async fn sse_get_forwards_stream_and_emits_sse_event() {
        use axum::routing::get;
        let upstream = Router::new().route(
            "/mcp",
            get(|| async {
                (
                    [("content-type", "text/event-stream")],
                    "data: {\"ping\":1}\n\n",
                )
            }),
        );
        let upstream_url = format!("{}/mcp", spawn_upstream(upstream).await);

        let (proxy, sink, handle) = build_test_proxy(&upstream_url, None);
        let uri: Uri = "http://proxy.test/mcp".parse().unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::ACCEPT,
            "text/event-stream".parse().unwrap(),
        );
        let resp = run(proxy, Method::GET, headers, uri, Bytes::new()).await;

        handle.shutdown().await;

        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let events = sink.snapshot();
        let req = only_request_event(&events);
        assert_eq!(req.method, "GET");
        assert_eq!(req.mcp_method.as_deref(), Some("SSE"));
        assert_eq!(req.note, "sse");
        assert!(req.session_id.is_none());
    }

    // ── Test #6 — Passthrough (non-MCP POST) rewrites upstream URLs ───────

    #[tokio::test]
    async fn passthrough_rewrites_upstream_url_in_json_response() {
        // Capture the upstream base URL inside the JSON response so we can
        // verify the UpstreamUrlMap substitution replaced it with the proxy
        // URL.
        use axum::routing::post;

        let upstream = Router::new().route(
            "/register",
            post(
                |axum::extract::State(base): axum::extract::State<String>| async move {
                    (
                        [("content-type", "application/json")],
                        format!("{{\"callback\":\"{base}/callback\"}}"),
                    )
                },
            ),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let upstream_base = format!("http://{addr}");
        let upstream_state = upstream_base.clone();
        let upstream_url = format!("{upstream_base}/register");
        tokio::spawn(async move {
            axum::serve(listener, upstream.with_state(upstream_state))
                .await
                .unwrap()
        });

        let (proxy, sink, handle) = build_test_proxy(&upstream_url, None);
        // Passthrough uses ctx.mcp_upstream as the forwarding base; set a
        // matching rewrite_config.mcp_upstream so UpstreamUrlMapMw knows the
        // base to substitute.
        {
            let mut cfg = proxy.rewrite_config.write().await;
            cfg.mcp_upstream = upstream_base.clone();
        }
        let uri: Uri = "http://proxy.test/register".parse().unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::CONTENT_TYPE,
            "application/x-www-form-urlencoded".parse().unwrap(),
        );
        let resp = run(
            proxy,
            Method::POST,
            headers,
            uri,
            Bytes::from("grant_type=foo"),
        )
        .await;

        handle.shutdown().await;

        let body = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
        let body_str = std::str::from_utf8(&body).unwrap();
        assert!(
            body_str.contains("https://proxy.test/callback"),
            "upstream URL should be rewritten to proxy URL, got: {body_str}"
        );
        assert!(
            !body_str.contains(&upstream_base),
            "upstream URL leaked into response: {body_str}"
        );

        let events = sink.snapshot();
        let req = only_request_event(&events);
        assert!(req.mcp_method.is_none());
        assert!(req.session_id.is_none());
        assert_eq!(req.note, "rewritten");
    }

    // ── Test #7 — DELETE emits SessionEnd + removes session ───────────────

    #[tokio::test]
    async fn delete_with_session_id_ends_session_and_removes_it() {
        use crate::protocol::session::SessionStore;
        use axum::routing::delete;

        let upstream = Router::new().route("/mcp", delete(|| async { "" }));
        let upstream_url = format!("{}/mcp", spawn_upstream(upstream).await);

        let (proxy, sink, handle) = build_test_proxy(&upstream_url, None);
        proxy.sessions.create("sess-xyz").await;
        assert!(proxy.sessions.get("sess-xyz").await.is_some());

        let uri: Uri = "http://proxy.test/mcp".parse().unwrap();
        let mut headers = HeaderMap::new();
        headers.insert("mcp-session-id", "sess-xyz".parse().unwrap());
        let _ = run(proxy.clone(), Method::DELETE, headers, uri, Bytes::new()).await;

        handle.shutdown().await;

        assert!(
            proxy.sessions.get("sess-xyz").await.is_none(),
            "session should be removed after DELETE"
        );
        let end = sink
            .snapshot()
            .into_iter()
            .find_map(|e| match e {
                ProxyEvent::SessionEnd(s) => Some(s),
                _ => None,
            })
            .expect("SessionEnd event missing");
        assert_eq!(end.session_id, "sess-xyz");
    }

    // ── Test #8 — WidgetOverlay substitutes local HTML ────────────────────

    #[tokio::test]
    async fn resources_read_overlays_widget_html_from_static_source() {
        let tmp = tempfile::tempdir().unwrap();
        let widget_dir = tmp.path().join("src/question");
        std::fs::create_dir_all(&widget_dir).unwrap();
        std::fs::write(
            widget_dir.join("index.html"),
            "<html><body>LOCAL WIDGET</body></html>",
        )
        .unwrap();

        let upstream = Router::new().route(
            "/mcp",
            post(|| async {
                axum::Json(json!({
                    "jsonrpc": "2.0", "id": 1,
                    "result": {
                        "contents": [{
                            "uri": "ui://widget/question",
                            "mimeType": "text/html",
                            "text": "UPSTREAM PLACEHOLDER"
                        }]
                    }
                }))
            }),
        );
        let upstream_url = format!("{}/mcp", spawn_upstream(upstream).await);

        let widget_source = Some(WidgetSource::Static(
            tmp.path().to_string_lossy().to_string(),
        ));
        let (proxy, _sink, handle) = build_test_proxy(&upstream_url, widget_source);
        let (method, headers, uri, body) = post_mcp(
            "/mcp",
            json!({
                "jsonrpc": "2.0", "id": 1, "method": "resources/read",
                "params": {"uri": "ui://widget/question"}
            }),
        );
        let resp = run(proxy, method, headers, uri, body).await;

        handle.shutdown().await;

        let body = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        let body_str = std::str::from_utf8(&body).unwrap();
        assert!(
            body_str.contains("LOCAL WIDGET"),
            "response should carry local widget HTML: {body_str}"
        );
        assert!(
            !body_str.contains("UPSTREAM PLACEHOLDER"),
            "upstream text should be overwritten: {body_str}"
        );
    }

    // ── Test #9 — SSE-wrapped upstream response roundtrips with rewrite ───

    #[tokio::test]
    async fn sse_wrapped_response_is_rewritten_and_re_wrapped() {
        let upstream_json = json!({
            "jsonrpc": "2.0", "id": 1,
            "result": {
                "tools": [{
                    "name": "search",
                    "meta": {
                        "openai/widgetCSP": {
                            "connect_domains": ["http://localhost:9000"],
                            "resource_domains": [],
                            "frame_domains": []
                        }
                    }
                }]
            }
        });
        let upstream_body = format!(
            "event: message\ndata: {}\n\n",
            serde_json::to_string(&upstream_json).unwrap()
        );
        let upstream = Router::new().route(
            "/mcp",
            post(move || {
                let b = upstream_body.clone();
                async move { ([("content-type", "text/event-stream")], b) }
            }),
        );
        let upstream_url = format!("{}/mcp", spawn_upstream(upstream).await);

        let (proxy, sink, handle) = build_test_proxy(&upstream_url, None);
        let (method, headers, uri, body) = post_mcp(
            "/mcp",
            json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
        );
        let resp = run(proxy, method, headers, uri, body).await;

        handle.shutdown().await;

        let body = axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap();
        let body_str = std::str::from_utf8(&body).unwrap();
        assert!(
            body_str.starts_with("data: "),
            "response should be re-wrapped as SSE: {body_str}"
        );
        let inner = body_str.trim_start_matches("data: ").trim();
        let parsed: Value = serde_json::from_str(inner.split('\n').next().unwrap())
            .expect("inner SSE payload parses as JSON");
        let domains: Vec<&str> =
            parsed["result"]["tools"][0]["meta"]["openai/widgetCSP"]["connect_domains"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|v| v.as_str())
                .collect();
        assert!(
            domains.contains(&"https://proxy.test"),
            "inner JSON must be CSP-rewritten: {domains:?}"
        );
        assert!(
            !domains.iter().any(|d| d.contains("localhost")),
            "localhost should be stripped from CSP: {domains:?}"
        );

        let events = sink.snapshot();
        let req = only_request_event(&events);
        assert_eq!(req.note, "rewritten+sse");
    }

    // ── Local helper used by tests 4–9 ────────────────────────────────────

    fn only_request_event(events: &[ProxyEvent]) -> &crate::event::RequestEvent {
        events
            .iter()
            .find_map(|e| match e {
                ProxyEvent::Request(r) => Some(r.as_ref()),
                _ => None,
            })
            .expect("expected a Request event")
    }
}
