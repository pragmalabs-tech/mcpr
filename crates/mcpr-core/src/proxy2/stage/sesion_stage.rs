use async_trait::async_trait;
use axum::http::request::Parts as RequestParts;

use crate::{
    protocol::{
        Request,
        mcp::JsonRpcRequest,
        session::{SessionInfo, SessionStore, session_id_from_headers},
    },
    proxy2::{stage::types::RequestStage, state::ProxyState},
};

pub struct SessionStage;

#[async_trait]
impl RequestStage for SessionStage {
    async fn process(&self, request: Request, state: ProxyState) -> anyhow::Result<Request> {
        match &request {
            Request::Mcp(parts, rpc) => {
                track_mcp_request(parts, rpc, &state.sessions);
            }
            Request::McpBatch(parts, rpcs) => {
                for rpc in rpcs {
                    track_mcp_request(parts, rpc, &state.sessions);
                }
            }
            Request::Http(_) => todo!(),
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
    session_store: &SessionStore,
) -> Option<SessionInfo> {
    let session_id = session_id_from_headers(&parts.headers)?;
    session_store.track_request(session_id, request.id.clone(), request.parse_client_info())
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;

    use axum::http::Request as HttpReq;
    use serde_json::{Map, Value, json};

    use crate::protocol::mcp::{
        ClientMethod, JsonRpcVersion, LifecycleMethod, RequestId, ToolsMethod,
    };

    fn parts_with(headers: &[(&str, &str)]) -> RequestParts {
        let mut builder = HttpReq::builder().method("POST").uri("/");
        for (k, v) in headers {
            builder = builder.header(*k, *v);
        }
        builder.body(()).unwrap().into_parts().0
    }

    fn rpc_request(method: ClientMethod, id: RequestId) -> JsonRpcRequest {
        JsonRpcRequest {
            jsonrpc: JsonRpcVersion,
            id,
            method,
            params: None,
        }
    }

    fn initialize_request(id: RequestId, client_name: &str) -> JsonRpcRequest {
        let mut params = Map::<String, Value>::new();
        params.insert("clientInfo".into(), json!({"name": client_name}));
        JsonRpcRequest {
            jsonrpc: JsonRpcVersion,
            id,
            method: ClientMethod::Lifecycle(LifecycleMethod::Initialize),
            params: Some(params),
        }
    }

    #[test]
    fn track_mcp_request__creates_session_with_session_header() {
        let store = SessionStore::new();
        let parts = parts_with(&[("mcp-session-id", "sess-1")]);
        let req = rpc_request(ClientMethod::Tools(ToolsMethod::List), RequestId::Number(1));

        let info = track_mcp_request(&parts, &req, &store).unwrap();
        assert_eq!(info.id, "sess-1");
        assert_eq!(info.request_count, 1);
    }

    #[test]
    fn track_mcp_request__missing_session_header_returns_none() {
        let store = SessionStore::new();
        let parts = parts_with(&[]);
        let req = rpc_request(ClientMethod::Tools(ToolsMethod::List), RequestId::Number(1));

        assert!(track_mcp_request(&parts, &req, &store).is_none());
        assert!(store.list_sessions().is_empty());
    }

    #[test]
    fn track_mcp_request__duplicate_request_id_returns_none() {
        let store = SessionStore::new();
        let parts = parts_with(&[("mcp-session-id", "sess-1")]);
        let req = rpc_request(ClientMethod::Tools(ToolsMethod::List), RequestId::Number(1));

        track_mcp_request(&parts, &req, &store).unwrap();
        assert!(track_mcp_request(&parts, &req, &store).is_none());
    }

    #[test]
    fn track_mcp_request__captures_client_info_from_initialize() {
        let store = SessionStore::new();
        let parts = parts_with(&[("mcp-session-id", "sess-1")]);
        let req = initialize_request(RequestId::Number(1), "Claude Code");

        let info = track_mcp_request(&parts, &req, &store).unwrap();
        assert_eq!(info.client_info.as_ref().unwrap().name, "Claude Code");
    }

    #[test]
    fn track_mcp_request__appends_to_existing_session() {
        let store = SessionStore::new();
        let parts = parts_with(&[("mcp-session-id", "sess-1")]);

        track_mcp_request(
            &parts,
            &rpc_request(ClientMethod::Tools(ToolsMethod::List), RequestId::Number(1)),
            &store,
        );
        let info = track_mcp_request(
            &parts,
            &rpc_request(ClientMethod::Tools(ToolsMethod::List), RequestId::Number(2)),
            &store,
        )
        .unwrap();
        assert_eq!(info.request_count, 2);
        assert_eq!(
            info.request_ids,
            vec![RequestId::Number(1), RequestId::Number(2)]
        );
    }
}
