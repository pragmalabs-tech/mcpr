use async_trait::async_trait;
use axum::http::request::Parts as RequestParts;

use crate::{
    event::ProxyEvent,
    protocol::{Request, mcp::JsonRpcRequest, session::session_id_from_headers},
    proxy2::{stage::types::RequestStage, state::ProxyState},
};

pub struct SessionStage;

#[async_trait]
impl RequestStage for SessionStage {
    async fn process(&self, request: Request, state: ProxyState) -> anyhow::Result<Request> {
        match &request {
            Request::Mcp(parts, rpc) => {
                track_mcp_request(parts, rpc, &state);
            }
            Request::McpBatch(parts, rpcs) => {
                for rpc in rpcs {
                    track_mcp_request(parts, rpc, &state);
                }
            }
            Request::Http(_) => (),
        }

        Ok(request)
    }
}

/// Observe a single MCP request: extract `(session_id, request_id, client_info)`
/// and forward to the store. Returns `Some` on a state change worth emitting,
/// `None` when there's no session header yet (e.g. the `initialize` request,
/// which gets its session from the response) or the request id is a duplicate.
fn track_mcp_request(
    parts: &RequestParts,
    request: &JsonRpcRequest,
    state: &ProxyState,
) -> Option<()> {
    let session_id = session_id_from_headers(&parts.headers)?;
    let new_change = state.sessions.track_request(
        session_id,
        request.id.clone(),
        request.parse_client_info(),
    )?;

    state
        .event_bus
        .emit(ProxyEvent::Session(Box::new(new_change)));

    Some(())
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;

    use std::sync::{Arc, Mutex};

    use axum::http::Request as HttpReq;

    use crate::event::{EventBusHandle, EventManager, EventSink};
    use crate::protocol::mcp::{ClientMethod, JsonRpcVersion, RequestId, ToolsMethod};
    use crate::protocol::session::SessionStore;
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

    fn parts_with(headers: &[(&str, &str)]) -> RequestParts {
        let mut builder = HttpReq::builder().method("POST").uri("/");
        for (k, v) in headers {
            builder = builder.header(*k, *v);
        }
        builder.body(()).unwrap().into_parts().0
    }

    fn tools_list_request() -> JsonRpcRequest {
        JsonRpcRequest {
            jsonrpc: JsonRpcVersion,
            id: RequestId::Number(1),
            method: ClientMethod::Tools(ToolsMethod::List),
            params: None,
        }
    }

    #[tokio::test]
    async fn track_mcp_request__forwards_session_header_and_emits() {
        let (state, sink, handle) = state_with_sink();
        let parts = parts_with(&[("mcp-session-id", "sess-1")]);

        assert!(track_mcp_request(&parts, &tools_list_request(), &state).is_some());

        let info = state.sessions.get_session("sess-1").unwrap();
        assert_eq!(info.request_count, 1);

        handle.shutdown().await;
        let events = sink.snapshot();
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], ProxyEvent::Session(_)));
    }

    #[tokio::test]
    async fn track_mcp_request__missing_session_header_skips_store_and_emit() {
        let (state, sink, handle) = state_with_sink();
        let parts = parts_with(&[]);

        assert!(track_mcp_request(&parts, &tools_list_request(), &state).is_none());
        assert!(state.sessions.list_sessions().is_empty());

        handle.shutdown().await;
        assert!(sink.snapshot().is_empty());
    }
}
