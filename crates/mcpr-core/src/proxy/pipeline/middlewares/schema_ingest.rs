//! Response-side middleware: spawn fire-and-forget schema ingest on
//! list-method result responses.
//!
//! Reads the originating method from `cx.working.request_method`
//! (stashed by `SessionTouchMiddleware`), reconstructs the request
//! envelope the ingest task needs, and hands ownership to the spawned
//! task. The hot path never waits for merge / hash / store.

use async_trait::async_trait;
use serde_json::Value;

use crate::event::{ProxyEvent, SchemaVersionCreatedEvent};
use crate::protocol::mcp::{MessageKind, ServerKind};
use crate::protocol::schema as proto_schema;
use crate::proxy::pipeline::middleware::ResponseMiddleware;
use crate::proxy::pipeline::values::{Context, Response};

pub struct SchemaIngestMiddleware;

#[async_trait]
impl ResponseMiddleware for SchemaIngestMiddleware {
    fn name(&self) -> &'static str {
        "schema_ingest"
    }

    async fn on_response(&self, resp: Response, cx: &mut Context) -> Response {
        let message = match &resp {
            Response::McpBuffered { message, .. } => message,
            _ => return resp,
        };
        if !matches!(message.kind, MessageKind::Server(ServerKind::Result)) {
            return resp;
        }
        let Some(method) = cx.working.request_method.as_ref() else {
            return resp;
        };
        if !proto_schema::is_schema_method(method) {
            return resp;
        }
        let Some(method_str) = method.as_str() else {
            return resp;
        };
        let Some(result_val) = message.envelope.result_as::<Value>() else {
            return resp;
        };
        // `SchemaManager::ingest` reads `response_body["result"]`, so
        // pass the full JSON-RPC shape. The request envelope is best-
        // effort — we don't have the original request params on the
        // response side yet, so the `params` field is `null`. That
        // matches today's behavior when the incoming request omits
        // `params` (e.g. vanilla `tools/list`).
        let response_val = serde_json::json!({
            "jsonrpc": "2.0",
            "result": result_val,
        });
        let req_val = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method_str,
        });

        let state = cx.intake.proxy.clone();
        let bus = state.event_bus.clone();
        let upstream_id = state.name.clone();
        let upstream_url = state.mcp_upstream.clone();
        let method_owned = method_str.to_string();

        state
            .schema_manager
            .spawn_ingest(method_owned, req_val, response_val, move |version| {
                bus.emit(ProxyEvent::SchemaVersionCreated(
                    SchemaVersionCreatedEvent {
                        ts: chrono::Utc::now().timestamp_millis(),
                        upstream_id,
                        upstream_url,
                        method: version.method.clone(),
                        version: version.version,
                        version_id: version.id.to_string(),
                        content_hash: version.content_hash.clone(),
                        payload: (*version.payload).clone(),
                    },
                ));
            });

        resp
    }
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;

    use axum::http::StatusCode;

    use crate::protocol::mcp::{ClientMethod, ToolsMethod};
    use crate::proxy::pipeline::middlewares::test_support::{
        mcp_buffered_response, set_request_method, test_context, test_proxy_with_sink,
    };

    #[tokio::test]
    async fn on_response__non_buffered_passthrough() {
        let (proxy, sink, handle) = test_proxy_with_sink();
        let mut cx = test_context(proxy.clone());
        set_request_method(&mut cx, ClientMethod::Tools(ToolsMethod::List));
        let resp = Response::Upstream502 {
            reason: "boom".into(),
        };

        let resp = SchemaIngestMiddleware.on_response(resp, &mut cx).await;
        assert!(matches!(resp, Response::Upstream502 { .. }));
        proxy.schema_manager.wait_idle().await;
        handle.shutdown().await;
        assert!(sink.snapshot().is_empty());
    }

    #[tokio::test]
    async fn on_response__no_request_method_passthrough() {
        let (proxy, sink, handle) = test_proxy_with_sink();
        let mut cx = test_context(proxy.clone());
        let resp = mcp_buffered_response(
            r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[]}}"#,
            StatusCode::OK,
        );

        SchemaIngestMiddleware.on_response(resp, &mut cx).await;
        proxy.schema_manager.wait_idle().await;
        handle.shutdown().await;
        assert!(sink.snapshot().is_empty());
    }

    #[tokio::test]
    async fn on_response__non_schema_method_passthrough() {
        let (proxy, sink, handle) = test_proxy_with_sink();
        let mut cx = test_context(proxy.clone());
        set_request_method(&mut cx, ClientMethod::Tools(ToolsMethod::Call));
        let resp = mcp_buffered_response(
            r#"{"jsonrpc":"2.0","id":1,"result":{"content":[]}}"#,
            StatusCode::OK,
        );

        SchemaIngestMiddleware.on_response(resp, &mut cx).await;
        proxy.schema_manager.wait_idle().await;
        handle.shutdown().await;
        assert!(sink.snapshot().is_empty());
    }

    #[tokio::test]
    async fn on_response__server_notification_passthrough() {
        let (proxy, sink, handle) = test_proxy_with_sink();
        let mut cx = test_context(proxy.clone());
        set_request_method(&mut cx, ClientMethod::Tools(ToolsMethod::List));
        let resp = mcp_buffered_response(
            r#"{"jsonrpc":"2.0","method":"notifications/tools/list_changed"}"#,
            StatusCode::OK,
        );

        SchemaIngestMiddleware.on_response(resp, &mut cx).await;
        proxy.schema_manager.wait_idle().await;
        handle.shutdown().await;
        assert!(sink.snapshot().is_empty());
    }

    #[tokio::test]
    async fn on_response__tools_list_spawns_and_emits() {
        let (proxy, sink, handle) = test_proxy_with_sink();
        let mut cx = test_context(proxy.clone());
        set_request_method(&mut cx, ClientMethod::Tools(ToolsMethod::List));
        let resp = mcp_buffered_response(
            r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[{"name":"one"}]}}"#,
            StatusCode::OK,
        );

        SchemaIngestMiddleware.on_response(resp, &mut cx).await;
        proxy.schema_manager.wait_idle().await;
        handle.shutdown().await;

        let events = sink.snapshot();
        let got_schema = events
            .iter()
            .any(|e| matches!(e, ProxyEvent::SchemaVersionCreated(v) if v.method == "tools/list"));
        assert!(got_schema, "expected SchemaVersionCreated for tools/list");
    }
}
