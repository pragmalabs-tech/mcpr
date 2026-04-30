//! Response stage that emits an MCP schema event per tool seen.
//!
//! Dispatch is method-driven: the originating MCP method comes in via
//! [`RequestContext::client_methods`] (id → method, populated at
//! pipeline entry). We only walk `result.tools[]` when the response's
//! id maps to `tools/list`. Same logic for both single MCP responses
//! and batch entries.
//!
//! Each entry is deserialized into a [`Tool`] and emitted as a
//! [`ChangeSchema::Tool`] event.
//!
//! The proxy keeps no schema state — the bus is the only sink. Sinks
//! that need dedup (cloud, store) own their own `Schema` and call
//! `add_tool` themselves.
//!
//! Malformed tool entries are skipped silently: an upstream that emits a
//! tool without `inputSchema` shouldn't fail the response on its way
//! back to the client.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use crate::{
    event::ProxyEvent,
    protocol::{
        Response,
        mcp::{ClientMethod, JsonRpcResponse, JsonRpcResult, ToolsMethod},
        schema::{ChangeSchema, Reason, Tool},
    },
    proxy2::{
        stage::types::{RequestContext, ResponseStage},
        state::ProxyState,
    },
};

pub struct SchemaTrackingStage;

#[async_trait]
impl ResponseStage for SchemaTrackingStage {
    async fn process(
        &self,
        res: Response,
        request_ctx: RequestContext,
        state: ProxyState,
    ) -> anyhow::Result<Response> {
        match &res {
            Response::Mcp(_, JsonRpcResult::Response(r)) => {
                track_tools_list(r, &request_ctx, &state);
            }
            Response::McpBatch(_, items) => {
                for item in items {
                    if let JsonRpcResult::Response(r) = item {
                        track_tools_list(r, &request_ctx, &state);
                    }
                }
            }
            _ => {}
        }
        Ok(res)
    }
}

fn track_tools_list(response: &JsonRpcResponse, request_ctx: &RequestContext, state: &ProxyState) {
    let method = request_ctx.get_method(&response.id);

    if !is_tools_list(method) {
        return;
    }

    let Some(result) = response.result.as_ref() else {
        return;
    };

    tracking_tools(result, state);
}

fn is_tools_list(method: Option<&ClientMethod>) -> bool {
    matches!(method, Some(ClientMethod::Tools(ToolsMethod::List)))
}

fn tracking_tools(result: &Value, state: &ProxyState) {
    let Some(tools) = result.get("tools").and_then(|t| t.as_array()) else {
        return;
    };

    for tool_value in tools {
        let Ok(tool) = serde_json::from_value::<Tool>(tool_value.clone()) else {
            continue;
        };
        state
            .event_bus
            .emit(ProxyEvent::Schema(Arc::new(ChangeSchema::Tool(
                Reason::Added,
                tool,
            ))));
    }
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;

    use std::collections::HashMap;
    use std::sync::Mutex;

    use axum::http::response::Parts as ResponseParts;
    use serde_json::json;

    use crate::event::{EventBusHandle, EventManager, EventSink};
    use crate::protocol::mcp::{
        JsonRpcError, JsonRpcResponse, JsonRpcVersion, PromptsMethod, RequestId,
    };
    use crate::protocol::schema::ChangeSchema;
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

    fn empty_response_parts() -> ResponseParts {
        axum::http::Response::new(()).into_parts().0
    }

    /// `Response::Mcp` with id 1 and the given result.
    fn mcp_response(result: Value) -> Response {
        Response::Mcp(
            empty_response_parts(),
            JsonRpcResult::Response(JsonRpcResponse {
                jsonrpc: JsonRpcVersion,
                id: RequestId::Number(1),
                result: Some(result),
            }),
        )
    }

    /// `RequestContext` mapping id 1 to the given method.
    fn ctx_for(method: ClientMethod) -> RequestContext {
        let mut m = HashMap::new();
        m.insert(RequestId::Number(1), method);
        RequestContext { client_methods: m }
    }

    fn tools_list_ctx() -> RequestContext {
        ctx_for(ClientMethod::Tools(ToolsMethod::List))
    }

    fn schema_changes(events: &[ProxyEvent]) -> Vec<&ChangeSchema> {
        events
            .iter()
            .filter_map(|e| match e {
                ProxyEvent::Schema(c) => Some(c.as_ref()),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn process__tools_list_emits_schema_event_per_new_tool() {
        let (state, sink, handle) = state_with_sink();
        let resp = mcp_response(json!({
            "tools": [
                {"name": "search", "inputSchema": {"type": "object"}},
                {"name": "lookup", "inputSchema": {"type": "object"}}
            ]
        }));

        SchemaTrackingStage
            .process(resp, tools_list_ctx(), state)
            .await
            .unwrap();
        handle.shutdown().await;

        let events = sink.snapshot();
        let changes = schema_changes(&events);
        assert_eq!(changes.len(), 2);
        for change in &changes {
            assert!(matches!(change, ChangeSchema::Tool(Reason::Added, _)));
        }
    }

    #[tokio::test]
    async fn process__tools_list_emits_every_call_without_dedup() {
        // The stage is stateless — sinks own dedup. Two identical responses
        // should produce two events.
        let (state, sink, handle) = state_with_sink();
        let resp = || {
            mcp_response(json!({
                "tools": [{"name": "search", "inputSchema": {"type": "object"}}]
            }))
        };

        SchemaTrackingStage
            .process(resp(), tools_list_ctx(), state.clone())
            .await
            .unwrap();
        SchemaTrackingStage
            .process(resp(), tools_list_ctx(), state)
            .await
            .unwrap();
        handle.shutdown().await;

        assert_eq!(schema_changes(&sink.snapshot()).len(), 2);
    }

    #[tokio::test]
    async fn process__non_tools_list_method_skips_tracking_even_with_tools_field() {
        let (state, sink, handle) = state_with_sink();
        let resp = mcp_response(json!({
            "tools": [{"name": "search", "inputSchema": {"type": "object"}}]
        }));

        SchemaTrackingStage
            .process(
                resp,
                ctx_for(ClientMethod::Prompts(PromptsMethod::List)),
                state,
            )
            .await
            .unwrap();
        handle.shutdown().await;

        assert!(schema_changes(&sink.snapshot()).is_empty());
    }

    #[tokio::test]
    async fn process__empty_request_context_emits_nothing() {
        // No id→method entries (e.g. Request::Http upstream of this stage).
        let (state, sink, handle) = state_with_sink();
        let resp = mcp_response(json!({
            "tools": [{"name": "search", "inputSchema": {"type": "object"}}]
        }));

        SchemaTrackingStage
            .process(resp, RequestContext::default(), state)
            .await
            .unwrap();
        handle.shutdown().await;

        assert!(schema_changes(&sink.snapshot()).is_empty());
    }

    #[tokio::test]
    async fn process__missing_tools_array_emits_nothing() {
        let (state, sink, handle) = state_with_sink();
        let resp = mcp_response(json!({"prompts": []}));

        SchemaTrackingStage
            .process(resp, tools_list_ctx(), state)
            .await
            .unwrap();
        handle.shutdown().await;

        assert!(schema_changes(&sink.snapshot()).is_empty());
    }

    #[tokio::test]
    async fn process__malformed_tool_entry_is_skipped() {
        let (state, sink, handle) = state_with_sink();
        let resp = mcp_response(json!({
            "tools": [
                {"name": "ok", "inputSchema": {"type": "object"}},
                {"name": "broken"}
            ]
        }));

        SchemaTrackingStage
            .process(resp, tools_list_ctx(), state)
            .await
            .unwrap();
        handle.shutdown().await;

        assert_eq!(schema_changes(&sink.snapshot()).len(), 1);
    }

    #[tokio::test]
    async fn process__error_response_emits_nothing() {
        let (state, sink, handle) = state_with_sink();
        let resp = Response::Mcp(
            empty_response_parts(),
            JsonRpcResult::Error(JsonRpcError {
                code: -32603,
                message: "boom".into(),
                data: None,
            }),
        );

        SchemaTrackingStage
            .process(resp, tools_list_ctx(), state)
            .await
            .unwrap();
        handle.shutdown().await;

        assert!(schema_changes(&sink.snapshot()).is_empty());
    }

    #[tokio::test]
    async fn process__mcp_batch_tracks_only_tools_list_items_by_id() {
        let (state, sink, handle) = state_with_sink();
        let item = |id: i64, name: &str| {
            JsonRpcResult::Response(JsonRpcResponse {
                jsonrpc: JsonRpcVersion,
                id: RequestId::Number(id),
                result: Some(json!({
                    "tools": [{"name": name, "inputSchema": {"type": "object"}}]
                })),
            })
        };
        let resp = Response::McpBatch(
            empty_response_parts(),
            vec![item(1, "search"), item(2, "lookup")],
        );
        // id 1 is tools/list; id 2 is prompts/list — only id 1's tools track.
        let mut methods: HashMap<RequestId, ClientMethod> = HashMap::new();
        methods.insert(RequestId::Number(1), ClientMethod::Tools(ToolsMethod::List));
        methods.insert(
            RequestId::Number(2),
            ClientMethod::Prompts(PromptsMethod::List),
        );
        let ctx = RequestContext {
            client_methods: methods,
        };

        SchemaTrackingStage.process(resp, ctx, state).await.unwrap();
        handle.shutdown().await;

        let events = sink.snapshot();
        let changes = schema_changes(&events);
        assert_eq!(changes.len(), 1);
        assert!(matches!(
            changes[0],
            ChangeSchema::Tool(Reason::Added, t) if t.name == "search"
        ));
    }
}
