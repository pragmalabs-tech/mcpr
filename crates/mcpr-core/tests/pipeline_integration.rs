//! End-to-end pipeline integration tests.
//!
//! These tests exercise the full pipeline (intake → request chain →
//! router → transport → response chain → emit → `IntoResponse`) against
//! real axum upstreams. They assert on `RequestEvent` wire shape and
//! session-lifecycle event contents — `mcpr-cloud/backend/` consumers
//! rely on both.
//!
//! Run with: `cargo test -p mcpr-core --test pipeline_integration`.

#![allow(non_snake_case)]

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::http::{HeaderMap, Method, Uri};
use axum::response::IntoResponse;
use axum::{Router, routing::post};
use serde_json::{Value, json};

use mcpr_core::event::SchemaVersionCreatedEvent;
use mcpr_core::event::{EventBusHandle, EventManager, EventSink, ProxyEvent, RequestEvent};
use mcpr_core::protocol::schema_manager::{MemorySchemaStore, SchemaManager};
use mcpr_core::protocol::session::MemorySessionStore;
use mcpr_core::proxy::forwarding::UpstreamClient;
use mcpr_core::proxy::intake::from_axum_parts;
use mcpr_core::proxy::pipeline::values::{Context, Intake, Working};
use mcpr_core::proxy::{
    CspConfig, ProxyPipeline, ProxyState, RewriteConfig, build_default_pipeline, emit,
    new_shared_health,
};

// ── Capturing sink ───────────────────────────────────────────────────

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

// ── Harness ──────────────────────────────────────────────────────────

async fn spawn_upstream(app: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

fn build_test_proxy(
    mcp_upstream: &str,
) -> (
    Arc<ProxyState>,
    Arc<ProxyPipeline>,
    CapturingSink,
    EventBusHandle,
) {
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
            request_timeout: Duration::from_secs(5),
        },
        max_request_body: 1 << 20,
        max_response_body: 1 << 20,
        max_concurrent_upstream: 10,
        rewrite_config: RewriteConfig {
            proxy_url: "https://proxy.test".to_string(),
            proxy_domain: "proxy.test".to_string(),
            mcp_upstream: mcp_upstream.to_string(),
            csp: CspConfig::default(),
        }
        .into_swap(),
        sessions: MemorySessionStore::new(),
        schema_manager: Arc::new(SchemaManager::new("test", MemorySchemaStore::new())),
        health: new_shared_health(),
        event_bus: handle.bus.clone(),
    });
    let pipeline = Arc::new(build_default_pipeline(proxy.rewrite_config.clone()));
    (proxy, pipeline, sink, handle)
}

/// Drive one request through the entire pipeline — the same sequence
/// that `mcpr-cli/src/proxy.rs::handle_request` executes. Reused across
/// every test below so all assertions exercise the production path.
async fn drive(
    proxy: Arc<ProxyState>,
    pipeline: Arc<ProxyPipeline>,
    method: Method,
    headers: HeaderMap,
    uri: Uri,
    body: Bytes,
) -> axum::response::Response {
    let start = Instant::now();
    let request_size = body.len();
    let path = uri.path().to_string();
    let http_method = method.clone();
    let req = from_axum_parts(method, headers, uri, body);
    let mut cx = Context {
        intake: Intake {
            start,
            proxy: proxy.clone(),
            http_method,
            path,
            request_size,
        },
        working: Working::default(),
    };
    let resp = pipeline.run(req, &mut cx).await;
    emit::emit(&cx, &resp);
    resp.into_response()
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

fn only_schema_event(events: &[ProxyEvent]) -> &SchemaVersionCreatedEvent {
    events
        .iter()
        .find_map(|e| match e {
            ProxyEvent::SchemaVersionCreated(s) => Some(s),
            _ => None,
        })
        .expect("expected a SchemaVersionCreated event")
}

fn only_request_event(events: &[ProxyEvent]) -> &RequestEvent {
    events
        .iter()
        .find_map(|e| match e {
            ProxyEvent::Request(r) => Some(r.as_ref()),
            _ => None,
        })
        .expect("expected a Request event")
}

// ── Test #1 — ordering canary ────────────────────────────────────────
//
// SchemaIngestMiddleware must run BEFORE CspRewriteMiddleware so the
// schema store captures the upstream's original CSP, not the proxy-
// rewritten one. If someone swaps those two in `build_default_pipeline`,
// this test fails loudly.

#[tokio::test]
async fn schema_ingest_sees_raw_upstream_csp_before_rewrite() {
    let upstream_body = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "result": {
            "tools": [{
                "name": "search",
                "_meta": {
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

    let (proxy, pipeline, sink, handle) = build_test_proxy(&upstream_url);
    let (method, headers, uri, body) = post_mcp(
        "/mcp",
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
    );
    let _ = drive(proxy.clone(), pipeline, method, headers, uri, body).await;

    proxy.schema_manager.wait_idle().await;
    handle.shutdown().await;

    let events = sink.snapshot();
    let schema = only_schema_event(&events);
    let captured: Vec<&str> =
        schema.payload["tools"][0]["_meta"]["openai/widgetCSP"]["connect_domains"]
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

// ── Test #2 — Initialize happy path ──────────────────────────────────

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

    let (proxy, pipeline, sink, handle) = build_test_proxy(&upstream_url);
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
    let _ = drive(proxy.clone(), pipeline, method, headers, uri, body).await;

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

    let req = only_request_event(&events);
    assert_eq!(req.mcp_method.as_deref(), Some("initialize"));
    assert_eq!(req.session_id.as_deref(), Some("sess-abc"));
    assert_eq!(req.client_name.as_deref(), Some("claude-desktop"));

    assert!(matches!(
        mcpr_core::proxy::lock_health(&proxy.health).mcp_status,
        mcpr_core::proxy::ConnectionStatus::Connected
    ));
}

// ── Test #3 — tools/list ingests schema and emits Request event ──────

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

    let (proxy, pipeline, sink, handle) = build_test_proxy(&upstream_url);
    let (method, headers, uri, body) = post_mcp(
        "/mcp",
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
    );
    let _ = drive(proxy.clone(), pipeline, method, headers, uri, body).await;

    proxy.schema_manager.wait_idle().await;
    handle.shutdown().await;

    let events = sink.snapshot();
    let schema = only_schema_event(&events);
    assert_eq!(schema.method, "tools/list");
    assert_eq!(schema.version, 1);

    let req = only_request_event(&events);
    assert_eq!(req.mcp_method.as_deref(), Some("tools/list"));
    assert_eq!(req.status, 200);
    assert_eq!(req.note, "rewritten");
}

// ── Test #3b — prompts/list schema-capture regression guard ──────────

#[tokio::test]
async fn prompts_list_emits_schema_version_event() {
    let upstream = Router::new().route(
        "/mcp",
        post(|| async {
            axum::Json(json!({
                "jsonrpc": "2.0", "id": 1,
                "result": {"prompts": [{"name": "summarize"}]}
            }))
        }),
    );
    let upstream_url = format!("{}/mcp", spawn_upstream(upstream).await);

    let (proxy, pipeline, sink, handle) = build_test_proxy(&upstream_url);
    let (method, headers, uri, body) = post_mcp(
        "/mcp",
        json!({"jsonrpc": "2.0", "id": 1, "method": "prompts/list"}),
    );
    let _ = drive(proxy.clone(), pipeline, method, headers, uri, body).await;

    proxy.schema_manager.wait_idle().await;
    handle.shutdown().await;

    let events = sink.snapshot();
    let schema = only_schema_event(&events);
    assert_eq!(schema.method, "prompts/list");
    assert_eq!(schema.version, 1);
    assert_eq!(schema.payload["prompts"][0]["name"], "summarize");
}

// ── Test #4 — 502 when upstream is unreachable ───────────────────────

#[tokio::test]
async fn upstream_unreachable_returns_502_and_emits_error_event() {
    let upstream_url = "http://127.0.0.1:1/mcp";

    let (proxy, pipeline, sink, handle) = build_test_proxy(upstream_url);
    let (method, headers, uri, body) = post_mcp(
        "/mcp",
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
    );
    let resp = drive(proxy, pipeline, method, headers, uri, body).await;

    handle.shutdown().await;

    assert_eq!(resp.status(), axum::http::StatusCode::BAD_GATEWAY);

    let events = sink.snapshot();
    let req = only_request_event(&events);
    assert_eq!(req.status, 502);
    assert_eq!(req.note, "upstream error");
}

// ── Test #5 — SSE GET streams from upstream ──────────────────────────

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

    let (proxy, pipeline, sink, handle) = build_test_proxy(&upstream_url);
    let uri: Uri = "http://proxy.test/mcp".parse().unwrap();
    let mut headers = HeaderMap::new();
    headers.insert(
        axum::http::header::ACCEPT,
        "text/event-stream".parse().unwrap(),
    );
    let resp = drive(proxy, pipeline, Method::GET, headers, uri, Bytes::new()).await;

    handle.shutdown().await;

    assert_eq!(resp.status(), axum::http::StatusCode::OK);
    let events = sink.snapshot();
    let req = only_request_event(&events);
    assert_eq!(req.method, "GET");
    assert_eq!(req.mcp_method.as_deref(), Some("SSE"));
    assert_eq!(req.note, "sse");
    assert!(req.session_id.is_none());
}

// ── Test #6 — DELETE emits SessionEnd + removes session ──────────────

#[tokio::test]
async fn delete_with_session_id_ends_session_and_removes_it() {
    use axum::routing::delete;
    use mcpr_core::protocol::session::SessionStore;

    let upstream = Router::new().route("/mcp", delete(|| async { "" }));
    let upstream_url = format!("{}/mcp", spawn_upstream(upstream).await);

    let (proxy, pipeline, sink, handle) = build_test_proxy(&upstream_url);
    proxy.sessions.create("sess-xyz").await;
    assert!(proxy.sessions.get("sess-xyz").await.is_some());

    let uri: Uri = "http://proxy.test/mcp".parse().unwrap();
    let mut headers = HeaderMap::new();
    headers.insert("mcp-session-id", "sess-xyz".parse().unwrap());
    let _ = drive(
        proxy.clone(),
        pipeline,
        Method::DELETE,
        headers,
        uri,
        Bytes::new(),
    )
    .await;

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

// ── Test #7 — resources/read forwards upstream text verbatim ─────────

#[tokio::test]
async fn resources_read_forwards_upstream_text_unchanged() {
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

    let (proxy, pipeline, _sink, handle) = build_test_proxy(&upstream_url);
    let (method, headers, uri, body) = post_mcp(
        "/mcp",
        json!({
            "jsonrpc": "2.0", "id": 1, "method": "resources/read",
            "params": {"uri": "ui://widget/question"}
        }),
    );
    let resp = drive(proxy, pipeline, method, headers, uri, body).await;

    handle.shutdown().await;

    let body = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    let body_str = std::str::from_utf8(&body).unwrap();
    assert!(
        body_str.contains("UPSTREAM PLACEHOLDER"),
        "upstream text must reach the client unchanged: {body_str}"
    );
}

// ── Test #8 — CSP rewrite survives widget removal ────────────────────

#[tokio::test]
async fn tools_list_csp_rewrite_survives_widget_removal() {
    let upstream_body = json!({
        "jsonrpc": "2.0", "id": 1,
        "result": {
            "tools": [{
                "name": "search",
                "_meta": {
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

    let (proxy, pipeline, _sink, handle) = build_test_proxy(&upstream_url);
    let (method, headers, uri, body) = post_mcp(
        "/mcp",
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
    );
    let resp = drive(proxy, pipeline, method, headers, uri, body).await;

    handle.shutdown().await;

    let body = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    let parsed: Value = serde_json::from_slice(&body).expect("response parses as JSON");
    let domains: Vec<&str> =
        parsed["result"]["tools"][0]["_meta"]["openai/widgetCSP"]["connect_domains"]
            .as_array()
            .expect("connect_domains array")
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
    assert!(
        domains.contains(&"https://proxy.test"),
        "proxy URL must be injected into rewritten CSP: {domains:?}"
    );
    assert!(
        !domains.iter().any(|d| d.contains("localhost")),
        "localhost must be stripped from rewritten CSP: {domains:?}"
    );
}

// ── Test #9 — SSE-wrapped buffered response roundtrips with rewrite ──

#[tokio::test]
async fn sse_wrapped_response_is_rewritten_and_re_wrapped() {
    let upstream_json = json!({
        "jsonrpc": "2.0", "id": 1,
        "result": {
            "tools": [{
                "name": "search",
                "_meta": {
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

    let (proxy, pipeline, sink, handle) = build_test_proxy(&upstream_url);
    let (method, headers, uri, body) = post_mcp(
        "/mcp",
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
    );
    let resp = drive(proxy, pipeline, method, headers, uri, body).await;

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
        parsed["result"]["tools"][0]["_meta"]["openai/widgetCSP"]["connect_domains"]
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
