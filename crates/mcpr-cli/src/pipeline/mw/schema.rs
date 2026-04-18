//! Schema ingest / stale-mark middleware.
//!
//! `SchemaIngestMw` feeds schema-method responses (tools/list,
//! resources/list, etc.) into the [`SchemaManager`] and emits
//! `SchemaVersionCreated` when the content changed.
//!
//! `StaleMarkMw` flips the `tools/list` stale flag when the response carries
//! a `notifications/tools/list_changed` notification.

use async_trait::async_trait;
use mcpr_core::event::{ProxyEvent, SchemaVersionCreatedEvent};
use mcpr_core::protocol as jsonrpc;
use mcpr_core::protocol::schema as proto_schema;
use serde_json::Value;

use super::ResponseMw;
use crate::pipeline::context::{RequestContext, ResponseContext};
use crate::state::ProxyState;

pub struct SchemaIngestMw;

#[async_trait]
impl ResponseMw for SchemaIngestMw {
    async fn on_response(
        &self,
        state: &ProxyState,
        req: &RequestContext,
        resp: &mut ResponseContext,
    ) {
        let (Some(mcp_method), Some(method_str), Some(json)) =
            (&req.mcp_method, &req.mcp_method_str, &resp.json)
        else {
            return;
        };
        if !proto_schema::is_schema_method(mcp_method) {
            return;
        }

        // Request body (the Value form) was parsed at POST time — we only
        // have raw bytes here, so re-parse just the wrapper. This matches
        // today's `ingest_schema(...)`.
        let req_val = req
            .jsonrpc
            .as_ref()
            .and_then(|_| serde_json::to_value(()).ok())
            .unwrap_or(Value::Null);
        // The ingest function today reads method + detail from the request
        // envelope. We don't thread the raw Bytes through, so rebuild a
        // minimal JSON-RPC request shape from ctx fields — ingest's only use
        // of the request is for the pagination cursor, which lives in params.
        let req_val = req
            .jsonrpc
            .as_ref()
            .and_then(|p| p.first_params().cloned())
            .map(|params| {
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "method": method_str,
                    "params": params,
                })
            })
            .unwrap_or(req_val);

        if let Some(version) = state
            .schema_manager
            .ingest(method_str, &req_val, json)
            .await
        {
            state.event_bus.emit(ProxyEvent::SchemaVersionCreated(
                SchemaVersionCreatedEvent {
                    ts: chrono::Utc::now().timestamp_millis(),
                    upstream_id: state.name.clone(),
                    upstream_url: state.mcp_upstream.clone(),
                    method: version.method.clone(),
                    version: version.version,
                    version_id: version.id.to_string(),
                    content_hash: version.content_hash.clone(),
                    payload: (*version.payload).clone(),
                },
            ));
        }
    }
}

pub struct StaleMarkMw;

#[async_trait]
impl ResponseMw for StaleMarkMw {
    async fn on_response(
        &self,
        state: &ProxyState,
        _req: &RequestContext,
        resp: &mut ResponseContext,
    ) {
        let Some(json) = &resp.json else {
            return;
        };
        if is_list_changed_response(json) {
            state.schema_manager.mark_stale(jsonrpc::TOOLS_LIST);
        }
    }
}

/// `true` if `response_body` carries a `notifications/tools/list_changed`
/// notification — either as a single JSON-RPC message or inside a batch array.
fn is_list_changed_response(response_body: &Value) -> bool {
    let is_notif = |v: &Value| {
        v.get("method").and_then(|m| m.as_str()) == Some(jsonrpc::NOTIFICATIONS_TOOLS_LIST_CHANGED)
    };
    is_notif(response_body)
        || response_body
            .as_array()
            .is_some_and(|arr| arr.iter().any(is_notif))
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use std::sync::Arc;

    use mcpr_core::protocol::McpMethod;
    use mcpr_core::protocol::schema_manager::{MemorySchemaStore, SchemaManager};
    use serde_json::json;

    use super::*;
    use crate::pipeline::context::{RequestContext, ResponseContext};

    fn test_state() -> ProxyState {
        use tokio::sync::RwLock;
        ProxyState {
            name: "test".to_string(),
            mcp_upstream: "http://upstream:9000".to_string(),
            upstream: mcpr_core::proxy::forwarding::UpstreamClient {
                http_client: reqwest::Client::builder().build().unwrap(),
                semaphore: Arc::new(tokio::sync::Semaphore::new(10)),
                request_timeout: std::time::Duration::from_secs(30),
            },
            max_request_body: 1024 * 1024,
            max_response_body: 1024 * 1024,
            rewrite_config: Arc::new(RwLock::new(mcpr_core::proxy::RewriteConfig {
                proxy_url: "http://localhost:0".to_string(),
                proxy_domain: "localhost".to_string(),
                mcp_upstream: "http://upstream:9000".to_string(),
                csp: mcpr_core::proxy::CspConfig::default(),
            })),
            widget_source: None,
            sessions: mcpr_core::protocol::session::MemorySessionStore::new(),
            schema_manager: Arc::new(SchemaManager::new("test", MemorySchemaStore::new())),
            health: mcpr_core::proxy::new_shared_health(),
            event_bus: mcpr_core::event::EventManager::new().start().bus,
        }
    }

    fn ctx_with(method: Option<McpMethod>) -> RequestContext {
        use std::time::Instant;
        RequestContext {
            start: Instant::now(),
            http_method: axum::http::Method::POST,
            path: "/mcp".into(),
            request_size: 0,
            wants_sse: false,
            session_id: None,
            jsonrpc: None,
            mcp_method_str: method.as_ref().map(|m| m.as_str().to_string()),
            mcp_method: method,
            tool: None,
            is_batch: false,
            client_info_from_init: None,
            client_name: None,
            client_version: None,
        }
    }

    fn resp_with(value: Value) -> ResponseContext {
        let mut r = ResponseContext::new(200, axum::http::HeaderMap::new(), vec![], None);
        r.json = Some(value);
        r
    }

    // ── is_list_changed_response ──

    #[test]
    fn is_list_changed__single_notification() {
        let v = json!({"jsonrpc": "2.0", "method": "notifications/tools/list_changed"});
        assert!(is_list_changed_response(&v));
    }

    #[test]
    fn is_list_changed__batch_with_notification() {
        let v = json!([
            {"jsonrpc": "2.0", "id": 1, "result": {}},
            {"jsonrpc": "2.0", "method": "notifications/tools/list_changed"}
        ]);
        assert!(is_list_changed_response(&v));
    }

    #[test]
    fn is_list_changed__unrelated_false() {
        let v = json!({"jsonrpc": "2.0", "id": 1, "result": {"tools": []}});
        assert!(!is_list_changed_response(&v));
    }

    #[test]
    fn is_list_changed__empty_batch_false() {
        let v = json!([]);
        assert!(!is_list_changed_response(&v));
    }

    // ── StaleMarkMw ──

    #[tokio::test]
    async fn stale_mark_mw__sets_flag_on_notification() {
        let state = test_state();
        let req = ctx_with(None);
        let mut resp =
            resp_with(json!({"jsonrpc": "2.0", "method": "notifications/tools/list_changed"}));
        assert!(!state.schema_manager.is_stale("tools/list"));
        StaleMarkMw.on_response(&state, &req, &mut resp).await;
        assert!(state.schema_manager.is_stale("tools/list"));
    }

    #[tokio::test]
    async fn stale_mark_mw__noop_on_unrelated() {
        let state = test_state();
        let req = ctx_with(None);
        let mut resp = resp_with(json!({"jsonrpc": "2.0", "id": 1, "result": {}}));
        StaleMarkMw.on_response(&state, &req, &mut resp).await;
        assert!(!state.schema_manager.is_stale("tools/list"));
    }

    // ── SchemaIngestMw ──

    #[tokio::test]
    async fn schema_ingest_mw__non_schema_method_is_noop() {
        let state = test_state();
        let req = ctx_with(Some(McpMethod::ToolsCall));
        let mut resp = resp_with(json!({"jsonrpc": "2.0", "id": 1, "result": {}}));
        SchemaIngestMw.on_response(&state, &req, &mut resp).await;
        assert!(state.schema_manager.latest("tools/list").await.is_none());
    }

    #[tokio::test]
    async fn schema_ingest_mw__error_response_is_noop() {
        let state = test_state();
        let req = ctx_with(Some(McpMethod::ToolsList));
        let mut resp = resp_with(
            json!({"jsonrpc": "2.0", "id": 1, "error": {"code": -32603, "message": "x"}}),
        );
        SchemaIngestMw.on_response(&state, &req, &mut resp).await;
        assert!(state.schema_manager.latest("tools/list").await.is_none());
    }

    #[tokio::test]
    async fn schema_ingest_mw__schema_method_creates_version() {
        let state = test_state();
        let req = ctx_with(Some(McpMethod::ToolsList));
        let mut resp = resp_with(json!({
            "jsonrpc": "2.0", "id": 1,
            "result": {"tools": [{"name": "search"}]}
        }));
        SchemaIngestMw.on_response(&state, &req, &mut resp).await;
        let latest = state.schema_manager.latest("tools/list").await.unwrap();
        assert_eq!(latest.version, 1);
        assert_eq!(latest.method, "tools/list");
    }

    #[tokio::test]
    async fn schema_ingest_mw__unchanged_payload_no_new_version() {
        let state = test_state();
        let req = ctx_with(Some(McpMethod::ToolsList));
        let body = json!({
            "jsonrpc": "2.0", "id": 1,
            "result": {"tools": [{"name": "search"}]}
        });
        let mut resp = resp_with(body.clone());
        SchemaIngestMw.on_response(&state, &req, &mut resp).await;
        let mut resp2 = resp_with(body);
        SchemaIngestMw.on_response(&state, &req, &mut resp2).await;
        let latest = state.schema_manager.latest("tools/list").await.unwrap();
        assert_eq!(latest.version, 1);
    }
}
