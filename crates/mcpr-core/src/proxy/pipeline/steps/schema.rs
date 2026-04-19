//! Schema ingest + stale-mark steps.
//!
//! Replaces `middleware::SchemaIngestMiddleware` and
//! `middleware::StaleMarkMiddleware` as plain functions — called
//! explicitly from the buffered handler after the response JSON has
//! been parsed.

use crate::event::{ProxyEvent, SchemaVersionCreatedEvent};
use crate::protocol as jsonrpc;
use crate::protocol::schema as proto_schema;
use serde_json::Value;

use crate::proxy::ProxyState;
use crate::proxy::pipeline::context::RequestContext;

/// Feed a schema-method response into the `SchemaManager` and emit
/// `SchemaVersionCreated` if the content changed. No-op unless the
/// method is one the schema manager tracks (`is_schema_method`).
pub async fn ingest(state: &ProxyState, ctx: &RequestContext, parsed: &Value) {
    let (Some(mcp_method), Some(method_str)) = (&ctx.mcp_method, &ctx.mcp_method_str) else {
        return;
    };
    if !proto_schema::is_schema_method(mcp_method) {
        return;
    }

    // The ingest function reads method + detail from a request envelope.
    // Rebuild a minimal JSON-RPC request shape from ctx fields — ingest's
    // only use of the request is the pagination cursor inside params.
    let req_val = ctx
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
        .unwrap_or(Value::Null);

    if let Some(version) = state
        .schema_manager
        .ingest(method_str, &req_val, parsed)
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

/// Flip the `tools/list` stale flag when the response carries a
/// `notifications/tools/list_changed` notification — either as a
/// single JSON-RPC message or inside a batch array.
pub fn mark_stale_if_listchanged(state: &ProxyState, parsed: &Value) {
    if is_list_changed_response(parsed) {
        state.schema_manager.mark_stale(jsonrpc::TOOLS_LIST);
    }
}

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
    use super::*;
    use serde_json::json;

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
}
