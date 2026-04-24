//! Shared `#[cfg(test)]` fixtures for middleware tests.
//!
//! Construct real subsystems (`MemorySessionStore`, `SchemaManager`,
//! `ProxyHealth`, `EventBus`) — they are cheap enough to instantiate per
//! test and avoid the divergence risk of hand-rolled mocks.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::http::{HeaderMap, Method, StatusCode};
use serde_json::Value;

use crate::event::{EventBusHandle, EventManager, EventSink, ProxyEvent};
use crate::protocol::schema_manager::{MemorySchemaStore, SchemaManager};
use crate::protocol::session::MemorySessionStore;
use crate::proxy::forwarding::UpstreamClient;
use crate::proxy::pipeline::envelope::JsonRpcEnvelope;
use crate::proxy::pipeline::message::{ClientKind, ClientMethod, McpMessage, MessageKind};
use crate::proxy::pipeline::stubs::SessionId;
use crate::proxy::pipeline::values::{
    Context, Envelope, Intake, McpRequest, McpTransport, Request, Response, Working,
};
use crate::proxy::{CspConfig, ProxyState, RewriteConfig, new_shared_health};

/// In-memory sink that records every event the bus dispatches. Tests
/// shut down the bus before snapshotting so the channel has drained.
#[derive(Clone, Default)]
pub(crate) struct CapturingSink {
    events: Arc<Mutex<Vec<ProxyEvent>>>,
}

impl CapturingSink {
    pub(crate) fn snapshot(&self) -> Vec<ProxyEvent> {
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

pub(crate) fn test_proxy_state() -> Arc<ProxyState> {
    let handle = EventManager::new().start();
    Arc::new(ProxyState {
        name: "middleware-test".into(),
        mcp_upstream: "http://upstream.test".into(),
        upstream: UpstreamClient {
            http_client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(1))
                .build()
                .unwrap(),
            request_timeout: Duration::from_secs(1),
        },
        max_request_body: 1 << 20,
        max_response_body: 1 << 20,
        max_concurrent_upstream: 1,
        rewrite_config: RewriteConfig {
            proxy_url: "https://proxy.test".into(),
            proxy_domain: "proxy.test".into(),
            mcp_upstream: "http://upstream.test".into(),
            csp: CspConfig::default(),
        }
        .into_swap(),
        sessions: MemorySessionStore::new(),
        schema_manager: Arc::new(SchemaManager::new(
            "middleware-test",
            MemorySchemaStore::new(),
        )),
        health: new_shared_health(),
        event_bus: handle.bus.clone(),
    })
}

pub(crate) fn test_proxy_with_sink() -> (Arc<ProxyState>, CapturingSink, EventBusHandle) {
    let sink = CapturingSink::default();
    let mut mgr = EventManager::new();
    mgr.register(Box::new(sink.clone()));
    let handle = mgr.start();
    let proxy = Arc::new(ProxyState {
        name: "middleware-test".into(),
        mcp_upstream: "http://upstream.test".into(),
        upstream: UpstreamClient {
            http_client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(1))
                .build()
                .unwrap(),
            request_timeout: Duration::from_secs(1),
        },
        max_request_body: 1 << 20,
        max_response_body: 1 << 20,
        max_concurrent_upstream: 1,
        rewrite_config: RewriteConfig {
            proxy_url: "https://proxy.test".into(),
            proxy_domain: "proxy.test".into(),
            mcp_upstream: "http://upstream.test".into(),
            csp: CspConfig::default(),
        }
        .into_swap(),
        sessions: MemorySessionStore::new(),
        schema_manager: Arc::new(SchemaManager::new(
            "middleware-test",
            MemorySchemaStore::new(),
        )),
        health: new_shared_health(),
        event_bus: handle.bus.clone(),
    });
    (proxy, sink, handle)
}

pub(crate) fn test_context(proxy: Arc<ProxyState>) -> Context {
    test_context_with_method(proxy, Method::POST)
}

pub(crate) fn test_context_with_method(proxy: Arc<ProxyState>, http_method: Method) -> Context {
    Context {
        intake: Intake {
            start: Instant::now(),
            proxy,
            http_method,
            path: "/mcp".into(),
            request_size: 0,
        },
        working: Working::default(),
    }
}

pub(crate) fn test_proxy_state_upstream(url: impl Into<String>) -> Arc<ProxyState> {
    let url = url.into();
    let handle = EventManager::new().start();
    Arc::new(ProxyState {
        name: "middleware-test".into(),
        mcp_upstream: url.clone(),
        upstream: UpstreamClient {
            http_client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(2))
                .build()
                .unwrap(),
            request_timeout: Duration::from_secs(5),
        },
        max_request_body: 1 << 20,
        max_response_body: 1 << 20,
        max_concurrent_upstream: 4,
        rewrite_config: RewriteConfig {
            proxy_url: "https://proxy.test".into(),
            proxy_domain: "proxy.test".into(),
            mcp_upstream: url,
            csp: CspConfig::default(),
        }
        .into_swap(),
        sessions: MemorySessionStore::new(),
        schema_manager: Arc::new(SchemaManager::new(
            "middleware-test",
            MemorySchemaStore::new(),
        )),
        health: new_shared_health(),
        event_bus: handle.bus.clone(),
    })
}

pub(crate) fn mcp_request(method: &str, params: Value, session: Option<&str>) -> Request {
    let body = if params.is_null() {
        format!(r#"{{"jsonrpc":"2.0","id":1,"method":"{method}"}}"#)
    } else {
        format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"{method}","params":{params}}}"#,
            params = params
        )
    };
    let envelope = JsonRpcEnvelope::parse(body.as_bytes()).unwrap();
    let kind = ClientKind::Request(ClientMethod::parse(method));
    Request::Mcp(McpRequest {
        transport: McpTransport::StreamableHttpPost,
        envelope,
        kind,
        headers: HeaderMap::new(),
        session_hint: session.map(SessionId::new),
    })
}

pub(crate) fn mcp_notification(method: &str, session: Option<&str>) -> Request {
    let body = format!(r#"{{"jsonrpc":"2.0","method":"{method}"}}"#);
    let envelope = JsonRpcEnvelope::parse(body.as_bytes()).unwrap();
    let kind = match envelope.method.as_deref() {
        Some(m) => {
            ClientKind::Notification(crate::proxy::pipeline::message::ClientNotifMethod::parse(m))
        }
        None => unreachable!("notification parsed without method"),
    };
    Request::Mcp(McpRequest {
        transport: McpTransport::StreamableHttpPost,
        envelope,
        kind,
        headers: HeaderMap::new(),
        session_hint: session.map(SessionId::new),
    })
}

pub(crate) fn mcp_buffered_response(body: &str, status: StatusCode) -> Response {
    let envelope = JsonRpcEnvelope::parse(body.as_bytes()).unwrap();
    let kind = MessageKind::Server(crate::proxy::pipeline::message::classify_server(&envelope));
    let message = McpMessage { envelope, kind };
    Response::McpBuffered {
        envelope: Envelope::Json,
        message,
        status,
        headers: HeaderMap::new(),
    }
}

pub(crate) fn mcp_buffered_response_with_header(
    body: &str,
    status: StatusCode,
    header_name: &'static str,
    header_value: &str,
) -> Response {
    let envelope = JsonRpcEnvelope::parse(body.as_bytes()).unwrap();
    let kind = MessageKind::Server(crate::proxy::pipeline::message::classify_server(&envelope));
    let message = McpMessage { envelope, kind };
    let mut headers = HeaderMap::new();
    headers.insert(header_name, header_value.parse().unwrap());
    Response::McpBuffered {
        envelope: Envelope::Json,
        message,
        status,
        headers,
    }
}

pub(crate) fn set_request_method(cx: &mut Context, m: ClientMethod) {
    cx.working.request_method = Some(m);
}

/// A DELETE-shaped MCP request. Intake synthesizes the same shape:
/// empty-body DELETE + `mcp-session-id` header becomes
/// `Request::Mcp(McpRequest { transport: StreamableHttpPost, .. })` with a
/// synthetic envelope so `SessionDeleteMiddleware` pattern-matches on
/// `Request::Mcp` + `cx.intake.http_method == DELETE`.
pub(crate) fn mcp_delete_request(session: Option<&str>) -> Request {
    let envelope = JsonRpcEnvelope::parse(br#"{"jsonrpc":"2.0","method":"delete"}"#).unwrap();
    let kind = ClientKind::Notification(
        crate::proxy::pipeline::message::ClientNotifMethod::Unknown("delete".into()),
    );
    Request::Mcp(McpRequest {
        transport: McpTransport::StreamableHttpPost,
        envelope,
        kind,
        headers: HeaderMap::new(),
        session_hint: session.map(SessionId::new),
    })
}
