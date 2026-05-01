//! Session lifecycle observer. Runs on the response side so it sees
//! both the inbound request context and the upstream response — the
//! only point where every fact about a session is known at once:
//!
//! - the `initialize` request id and `client_info`, captured into the
//!   context at pipeline entry,
//! - the `mcp-session-id` issued by the server in the response headers,
//! - the `serverInfo` from the response result.
//!
//! Behavior:
//! - Initialize response: `start_session` with all four fields, emit `Active`.
//! - Any other JSON-RPC response carrying a known `mcp-session-id`:
//!   `track_request`, bumps activity (auto-creates a stub if the proxy
//!   started mid-conversation).
//! - HTTP response to a `DELETE` carrying a session id: `end_session`,
//!   emit `Closed`.

use std::sync::Arc;

use async_trait::async_trait;
use axum::http::response::Parts as ResponseParts;

use crate::{
    event::ProxyEvent,
    protocol::{
        Response,
        mcp::{JsonRpcResponse, JsonRpcResult},
        session::{SessionId, session_id_from_headers},
    },
    proxy2::{
        stage::types::{RequestContext, ResponseStage},
        state::ProxyState,
    },
};

pub struct SessionTrackingStage;

#[async_trait]
impl ResponseStage for SessionTrackingStage {
    async fn process(
        &self,
        response: Response,
        request_ctx: RequestContext,
        state: ProxyState,
    ) -> anyhow::Result<Response> {
        match &response {
            Response::Mcp(parts, result) => {
                handle_mcp_result(parts, result, &request_ctx, &state);
            }
            Response::McpBatch(parts, results) => {
                for r in results {
                    handle_mcp_result(parts, r, &request_ctx, &state);
                }
            }
            Response::Http(_) => {
                end_session_if_close(&request_ctx, &state);
            }
        }
        Ok(response)
    }
}

/// Dispatch a single JSON-RPC result: initialize gets the
/// register-the-new-session path; anything else just bumps activity on
/// the session keyed by the inbound `mcp-session-id`.
fn handle_mcp_result(
    parts: &ResponseParts,
    result: &JsonRpcResult,
    ctx: &RequestContext,
    state: &ProxyState,
) -> Option<()> {
    let response = match result {
        JsonRpcResult::Response(r) => r,
        JsonRpcResult::Error(_) => return None,
    };

    if let Some((init_id, _)) = &ctx.initialize
        && init_id == &response.id
    {
        return start_session(parts, response, ctx, state);
    }

    let session_id = ctx.session_id.as_ref()?;
    let new_change = state
        .sessions
        .track_request(session_id.clone(), response.id.clone())?;
    state
        .event_bus
        .emit(ProxyEvent::Session(Arc::new(new_change)));
    Some(())
}

/// Register a session at the moment the `initialize` response carries
/// the server-issued `mcp-session-id`. Captures client info (from the
/// request) and server info (from the response) so subsequent requests
/// don't re-register.
fn start_session(
    parts: &ResponseParts,
    response: &JsonRpcResponse,
    ctx: &RequestContext,
    state: &ProxyState,
) -> Option<()> {
    let session_id: SessionId = session_id_from_headers(&parts.headers)?;
    let client_info = ctx.initialize.as_ref().map(|(_, ci)| ci.clone());
    let server_info = response.parse_server_info();

    let info =
        state
            .sessions
            .start_session(session_id, response.id.clone(), client_info, server_info)?;
    state.event_bus.emit(ProxyEvent::Session(Arc::new(info)));
    Some(())
}

/// Spec session close: client `DELETE`s with the session id. Hooked on
/// the response side so we observe the close only when the server
/// accepted the DELETE.
fn end_session_if_close(ctx: &RequestContext, state: &ProxyState) -> Option<()> {
    if !ctx.is_session_close {
        return None;
    }
    let session_id = ctx.session_id.as_ref()?;
    let closed = state.sessions.end_session(session_id)?;
    state.event_bus.emit(ProxyEvent::Session(Arc::new(closed)));
    Some(())
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;

    use std::sync::{Arc, Mutex};

    use axum::http::{HeaderValue, Method, Request as HttpReq};

    use crate::event::{EventBusHandle, EventManager, EventSink};
    use crate::protocol::Request;
    use crate::protocol::http_request::HttpRequest;
    use crate::protocol::mcp::{
        ClientMethod, JsonRpcRequest, JsonRpcResponse, JsonRpcVersion, LifecycleMethod, RequestId,
        ToolsMethod,
    };
    use crate::protocol::session::{SessionState, SessionStore};
    use crate::proxy2::state::InnerProxyState;

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

    fn state_with_sink() -> (ProxyState, CapturingSink, EventBusHandle) {
        let sink = CapturingSink::default();
        let mut mgr = EventManager::new();
        mgr.register(Box::new(sink.clone()));
        let handle = mgr.start();
        let state = Arc::new(InnerProxyState::new(
            handle.bus.clone(),
            SessionStore::new(),
        ));
        (state, sink, handle)
    }

    fn initialize_request_with_client_info() -> Request {
        let parts = HttpReq::builder()
            .method("POST")
            .uri("/")
            .body(())
            .unwrap()
            .into_parts()
            .0;
        let rpc = JsonRpcRequest {
            jsonrpc: JsonRpcVersion,
            id: RequestId::Number(0),
            method: ClientMethod::Lifecycle(LifecycleMethod::Initialize),
            params: Some(
                serde_json::from_value(serde_json::json!({
                    "clientInfo": { "name": "inspector-client", "version": "0.21.2" },
                    "protocolVersion": "2025-11-25",
                }))
                .unwrap(),
            ),
        };
        Request::Mcp(parts, rpc)
    }

    fn tools_list_request_with_session(session_id: &str) -> Request {
        let parts = HttpReq::builder()
            .method("POST")
            .uri("/")
            .header("mcp-session-id", session_id)
            .body(())
            .unwrap()
            .into_parts()
            .0;
        let rpc = JsonRpcRequest {
            jsonrpc: JsonRpcVersion,
            id: RequestId::Number(1),
            method: ClientMethod::Tools(ToolsMethod::List),
            params: None,
        };
        Request::Mcp(parts, rpc)
    }

    fn delete_request_with_session(session_id: &str) -> Request {
        let http: HttpRequest = HttpReq::builder()
            .method(Method::DELETE)
            .uri("/")
            .header("mcp-session-id", session_id)
            .body(axum::body::Bytes::new())
            .unwrap();
        Request::Http(http)
    }

    fn response_parts_with(headers: &[(&str, &str)]) -> ResponseParts {
        let mut builder = axum::http::Response::builder();
        for (k, v) in headers {
            builder = builder.header(*k, HeaderValue::from_str(v).unwrap());
        }
        builder.body(()).unwrap().into_parts().0
    }

    fn initialize_response_result(id: i64) -> JsonRpcResult {
        JsonRpcResult::Response(JsonRpcResponse {
            jsonrpc: JsonRpcVersion,
            id: RequestId::Number(id),
            result: Some(serde_json::json!({
                "capabilities": {},
                "protocolVersion": "2025-11-25",
                "serverInfo": { "name": "weather-app", "version": "1.0.0" },
            })),
        })
    }

    fn tools_list_result(id: i64) -> JsonRpcResult {
        JsonRpcResult::Response(JsonRpcResponse {
            jsonrpc: JsonRpcVersion,
            id: RequestId::Number(id),
            result: Some(serde_json::json!({"tools": []})),
        })
    }

    fn http_response_204() -> Response {
        let resp = axum::http::Response::builder()
            .status(204)
            .body(axum::body::Bytes::new())
            .unwrap();
        Response::Http(resp)
    }

    // ── initialize: response side creates session with full metadata ──

    #[tokio::test]
    async fn initialize_response__creates_session_with_client_and_server_info() {
        let (state, sink, handle) = state_with_sink();
        let req = initialize_request_with_client_info();
        let ctx = RequestContext::from_request(&req);
        let parts = response_parts_with(&[("mcp-session-id", "sess-xyz")]);
        let response = Response::Mcp(parts, initialize_response_result(0));

        SessionTrackingStage
            .process(response, ctx, state.clone())
            .await
            .unwrap();

        let info = state.sessions.get_session("sess-xyz").unwrap();
        let ci = info.client_info.unwrap();
        assert_eq!(ci.name, "inspector-client");
        assert_eq!(ci.version.as_deref(), Some("0.21.2"));
        let si = info.server_info.unwrap();
        assert_eq!(si.name, "weather-app");
        assert_eq!(si.version.as_deref(), Some("1.0.0"));
        assert_eq!(info.request_count, 1);
        assert_eq!(info.request_ids, vec![RequestId::Number(0)]);

        handle.shutdown().await;
        assert!(matches!(
            sink.snapshot().first(),
            Some(ProxyEvent::Session(_))
        ));
    }

    #[tokio::test]
    async fn initialize_response__without_session_header_is_noop() {
        let (state, sink, handle) = state_with_sink();
        let req = initialize_request_with_client_info();
        let ctx = RequestContext::from_request(&req);
        let parts = response_parts_with(&[]);
        let response = Response::Mcp(parts, initialize_response_result(0));

        SessionTrackingStage
            .process(response, ctx, state.clone())
            .await
            .unwrap();

        assert!(state.sessions.list_sessions().is_empty());
        handle.shutdown().await;
        assert!(sink.snapshot().is_empty());
    }

    // ── follow-up requests bump activity, don't reset metadata ────

    #[tokio::test]
    async fn followup_request__bumps_activity_keeps_initialize_metadata() {
        let (state, sink, handle) = state_with_sink();

        // Step 1: initialize creates the session.
        let init_req = initialize_request_with_client_info();
        let init_ctx = RequestContext::from_request(&init_req);
        let init_resp = Response::Mcp(
            response_parts_with(&[("mcp-session-id", "sess-xyz")]),
            initialize_response_result(0),
        );
        SessionTrackingStage
            .process(init_resp, init_ctx, state.clone())
            .await
            .unwrap();

        // Step 2: tools/list with the same session id.
        let req2 = tools_list_request_with_session("sess-xyz");
        let ctx2 = RequestContext::from_request(&req2);
        let resp2 = Response::Mcp(response_parts_with(&[]), tools_list_result(1));
        SessionTrackingStage
            .process(resp2, ctx2, state.clone())
            .await
            .unwrap();

        let info = state.sessions.get_session("sess-xyz").unwrap();
        assert_eq!(info.request_count, 2);
        assert_eq!(
            info.request_ids,
            vec![RequestId::Number(0), RequestId::Number(1)]
        );
        assert_eq!(info.client_info.unwrap().name, "inspector-client");
        assert_eq!(info.server_info.unwrap().name, "weather-app");

        handle.shutdown().await;
        let session_events = sink
            .snapshot()
            .into_iter()
            .filter(|e| matches!(e, ProxyEvent::Session(_)))
            .count();
        assert_eq!(session_events, 2);
    }

    #[tokio::test]
    async fn followup_request__without_existing_session_creates_stub() {
        // Edge: client sends a request with a session id we never saw initialize for
        // (e.g. proxy started mid-conversation). track_request creates a stub entry
        // with no client/server metadata so the session still surfaces.
        let (state, _sink, handle) = state_with_sink();
        let req = tools_list_request_with_session("sess-orphan");
        let ctx = RequestContext::from_request(&req);
        let resp = Response::Mcp(response_parts_with(&[]), tools_list_result(1));
        SessionTrackingStage
            .process(resp, ctx, state.clone())
            .await
            .unwrap();

        let info = state.sessions.get_session("sess-orphan").unwrap();
        assert!(info.client_info.is_none());
        assert!(info.server_info.is_none());
        assert_eq!(info.request_count, 1);
        handle.shutdown().await;
    }

    // ── DELETE closes the session at response time ────────────────

    #[tokio::test]
    async fn delete_response__ends_known_session() {
        let (state, sink, handle) = state_with_sink();
        state
            .sessions
            .start_session("sess-xyz".into(), RequestId::Number(0), None, None);

        let del_req = delete_request_with_session("sess-xyz");
        let ctx = RequestContext::from_request(&del_req);
        SessionTrackingStage
            .process(http_response_204(), ctx, state.clone())
            .await
            .unwrap();

        assert!(state.sessions.get_session("sess-xyz").is_none());
        handle.shutdown().await;
        let saw_closed = sink
            .snapshot()
            .into_iter()
            .any(|e| matches!(e, ProxyEvent::Session(s) if s.state == SessionState::Closed));
        assert!(saw_closed);
    }

    #[tokio::test]
    async fn non_delete_http_response__does_not_close() {
        let (state, _sink, handle) = state_with_sink();
        state
            .sessions
            .start_session("sess-xyz".into(), RequestId::Number(0), None, None);

        let http: HttpRequest = HttpReq::builder()
            .method(Method::POST)
            .uri("/")
            .header("mcp-session-id", "sess-xyz")
            .body(axum::body::Bytes::new())
            .unwrap();
        let ctx = RequestContext::from_request(&Request::Http(http));
        SessionTrackingStage
            .process(http_response_204(), ctx, state.clone())
            .await
            .unwrap();

        assert!(state.sessions.get_session("sess-xyz").is_some());
        handle.shutdown().await;
    }
}
