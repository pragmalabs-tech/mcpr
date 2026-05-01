//! Response stage that emits an MCP schema event per item seen in a
//! list response.
//!
//! Dispatch is method-driven: the originating MCP method comes in via
//! [`RequestContext::client_methods`] (id → method, populated at
//! pipeline entry). For each list method we know about we walk the
//! corresponding result array, deserialize each entry into the typed
//! struct from `protocol::schema`, and emit a [`ChangeSchema`] event.
//!
//! | Originating method            | Walks                         | Emits                            |
//! |-------------------------------|-------------------------------|----------------------------------|
//! | `tools/list`                  | `result.tools[]`              | `ChangeSchema::Tool`             |
//! | `prompts/list`                | `result.prompts[]`            | `ChangeSchema::Prompt`           |
//! | `resources/list`              | `result.resources[]`          | `ChangeSchema::Resource`         |
//! | `resources/templates/list`    | `result.resourceTemplates[]`  | `ChangeSchema::ResourceTemplate` |
//!
//! Same logic applies to single MCP responses and to each item in a
//! batch (id-matched against the batch context).
//!
//! The proxy keeps no schema state — the bus is the only sink. Sinks
//! that need dedup (cloud, store) own their own `Schema` and call
//! `add_*` themselves.
//!
//! Malformed entries are skipped silently: an upstream that emits a
//! tool without `inputSchema` (or a resource without `uri`, etc.)
//! shouldn't fail the response on its way back to the client.

use std::sync::Arc;

use async_trait::async_trait;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::{
    event::ProxyEvent,
    protocol::{
        Response,
        mcp::{
            ClientMethod, JsonRpcResponse, JsonRpcResult, PromptsMethod, ResourcesMethod,
            ToolsMethod,
        },
        schema::{ChangeSchema, Reason},
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
                track(r, &request_ctx, &state);
            }
            Response::McpBatch(_, items) => {
                for item in items {
                    if let JsonRpcResult::Response(r) = item {
                        track(r, &request_ctx, &state);
                    }
                }
            }
            _ => {}
        }
        Ok(res)
    }
}

/// Look up the originating method for this response's id and dispatch
/// to the right tracker. No-op if the id isn't in the context (e.g. an
/// HTTP request, or a method we don't track).
fn track(response: &JsonRpcResponse, request_ctx: &RequestContext, state: &ProxyState) {
    let Some(method) = request_ctx.get_method(&response.id) else {
        return;
    };
    let Some(result) = response.result.as_ref() else {
        return;
    };

    match method {
        ClientMethod::Tools(ToolsMethod::List) => {
            emit_each(result, "tools", state, |t| {
                ChangeSchema::Tool(Reason::Added, t)
            });
        }
        ClientMethod::Prompts(PromptsMethod::List) => {
            emit_each(result, "prompts", state, |p| {
                ChangeSchema::Prompt(Reason::Added, p)
            });
        }
        ClientMethod::Resources(ResourcesMethod::List) => {
            emit_each(result, "resources", state, |r| {
                ChangeSchema::Resource(Reason::Added, r)
            });
        }
        ClientMethod::Resources(ResourcesMethod::TemplatesList) => {
            emit_each(result, "resourceTemplates", state, |rt| {
                ChangeSchema::ResourceTemplate(Reason::Added, rt)
            });
        }
        _ => {}
    }
}

/// Walk `result[array_key]` (if present), deserialize each entry as
/// `T`, wrap with `make_change`, and emit. Malformed entries are
/// skipped silently.
fn emit_each<T, F>(result: &Value, array_key: &str, state: &ProxyState, make_change: F)
where
    T: DeserializeOwned,
    F: Fn(T) -> ChangeSchema,
{
    let Some(items) = result.get(array_key).and_then(|v| v.as_array()) else {
        return;
    };
    for item in items {
        let Ok(value) = serde_json::from_value::<T>(item.clone()) else {
            continue;
        };
        state
            .event_bus
            .emit(ProxyEvent::Schema(Arc::new(make_change(value))));
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
        JsonRpcError, JsonRpcErrorResponse, JsonRpcResponse, JsonRpcVersion, RequestId,
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
        RequestContext {
            client_methods: m,
            ..Default::default()
        }
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
            JsonRpcResult::Error(JsonRpcErrorResponse {
                jsonrpc: JsonRpcVersion,
                id: RequestId::Number(1),
                error: JsonRpcError {
                    code: -32603,
                    message: "boom".into(),
                    data: None,
                },
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
    async fn process__mcp_batch_dispatches_each_item_by_method() {
        let (state, sink, handle) = state_with_sink();
        let item = |id: i64, payload: Value| {
            JsonRpcResult::Response(JsonRpcResponse {
                jsonrpc: JsonRpcVersion,
                id: RequestId::Number(id),
                result: Some(payload),
            })
        };
        let resp = Response::McpBatch(
            empty_response_parts(),
            vec![
                item(
                    1,
                    json!({"tools": [{"name": "search", "inputSchema": {"type": "object"}}]}),
                ),
                item(2, json!({"prompts": [{"name": "greet"}]})),
                item(3, json!({"resources": [{"uri": "file:///a", "name": "a"}]})),
            ],
        );
        let mut methods: HashMap<RequestId, ClientMethod> = HashMap::new();
        methods.insert(RequestId::Number(1), ClientMethod::Tools(ToolsMethod::List));
        methods.insert(
            RequestId::Number(2),
            ClientMethod::Prompts(PromptsMethod::List),
        );
        methods.insert(
            RequestId::Number(3),
            ClientMethod::Resources(ResourcesMethod::List),
        );
        let ctx = RequestContext {
            client_methods: methods,
            ..Default::default()
        };

        SchemaTrackingStage.process(resp, ctx, state).await.unwrap();
        handle.shutdown().await;

        let events = sink.snapshot();
        let changes = schema_changes(&events);
        assert_eq!(changes.len(), 3);
        assert!(changes.iter().any(|c| matches!(
            c,
            ChangeSchema::Tool(Reason::Added, t) if t.name == "search"
        )));
        assert!(changes.iter().any(|c| matches!(
            c,
            ChangeSchema::Prompt(Reason::Added, p) if p.name == "greet"
        )));
        assert!(changes.iter().any(|c| matches!(
            c,
            ChangeSchema::Resource(Reason::Added, r) if r.uri == "file:///a"
        )));
    }

    // ── prompts/list ────────────────────────────────────────────

    #[tokio::test]
    async fn process__prompts_list_emits_prompt_event_per_entry() {
        let (state, sink, handle) = state_with_sink();
        let resp = mcp_response(json!({
            "prompts": [
                {"name": "greet", "description": "say hi"},
                {"name": "summarize"}
            ]
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

        let events = sink.snapshot();
        let changes = schema_changes(&events);
        assert_eq!(changes.len(), 2);
        for change in &changes {
            assert!(matches!(change, ChangeSchema::Prompt(Reason::Added, _)));
        }
    }

    #[tokio::test]
    async fn process__prompts_list_skips_malformed_entry() {
        let (state, sink, handle) = state_with_sink();
        // Second entry is missing `name`, which `Prompt` requires.
        let resp = mcp_response(json!({
            "prompts": [
                {"name": "greet"},
                {"description": "no name"}
            ]
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

        assert_eq!(schema_changes(&sink.snapshot()).len(), 1);
    }

    // ── resources/list ──────────────────────────────────────────

    #[tokio::test]
    async fn process__resources_list_emits_resource_event_per_entry() {
        let (state, sink, handle) = state_with_sink();
        let resp = mcp_response(json!({
            "resources": [
                {"uri": "file:///a", "name": "a", "mimeType": "text/plain"},
                {"uri": "file:///b", "name": "b"}
            ]
        }));

        SchemaTrackingStage
            .process(
                resp,
                ctx_for(ClientMethod::Resources(ResourcesMethod::List)),
                state,
            )
            .await
            .unwrap();
        handle.shutdown().await;

        let events = sink.snapshot();
        let changes = schema_changes(&events);
        assert_eq!(changes.len(), 2);
        for change in &changes {
            assert!(matches!(change, ChangeSchema::Resource(Reason::Added, _)));
        }
    }

    // ── resources/templates/list ────────────────────────────────

    #[tokio::test]
    async fn process__resource_templates_list_emits_resource_template_event_per_entry() {
        let (state, sink, handle) = state_with_sink();
        let resp = mcp_response(json!({
            "resourceTemplates": [
                {"uriTemplate": "doc://{id}", "name": "doc"},
                {"uriTemplate": "ui://widget/{name}", "name": "widget"}
            ]
        }));

        SchemaTrackingStage
            .process(
                resp,
                ctx_for(ClientMethod::Resources(ResourcesMethod::TemplatesList)),
                state,
            )
            .await
            .unwrap();
        handle.shutdown().await;

        let events = sink.snapshot();
        let changes = schema_changes(&events);
        assert_eq!(changes.len(), 2);
        for change in &changes {
            assert!(matches!(
                change,
                ChangeSchema::ResourceTemplate(Reason::Added, _)
            ));
        }
    }

    #[tokio::test]
    async fn process__unsupported_method_emits_nothing() {
        // `tools/call` returns a `CallToolResult`, not a tool definition —
        // we don't track it.
        let (state, sink, handle) = state_with_sink();
        let resp = mcp_response(json!({"content": [{"type": "text", "text": "ok"}]}));

        SchemaTrackingStage
            .process(resp, ctx_for(ClientMethod::Tools(ToolsMethod::Call)), state)
            .await
            .unwrap();
        handle.shutdown().await;

        assert!(schema_changes(&sink.snapshot()).is_empty());
    }
}
