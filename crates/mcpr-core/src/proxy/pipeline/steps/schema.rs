//! Schema ingest + stale-mark steps.
//!
//! Replaces `middleware::SchemaIngestMiddleware` and
//! `middleware::StaleMarkMiddleware` as plain functions — called
//! explicitly from the buffered handler after the response JSON has
//! been parsed.

use crate::event::{EventBus, ProxyEvent, SchemaVersionCreatedEvent};
use crate::protocol as jsonrpc;
use crate::protocol::schema as proto_schema;
use serde_json::Value;

use crate::proxy::ProxyState;
use crate::proxy::pipeline::context::RequestContext;

/// Fire-and-forget version of schema ingest.
///
/// Checks method eligibility synchronously (no clone if the method is
/// not a schema method), builds a minimal JSON-RPC request envelope,
/// and hands ownership of the cloned response body to a spawned task.
/// Returns immediately — the hot path never waits for merge_pages,
/// hash_payload, or store.put_version.
///
/// The per-proxy `SchemaManager` tracks in-flight spawns; callers that
/// need to observe the resulting `SchemaVersionCreated` event (tests,
/// shutdown) should `schema_manager.wait_idle().await` before
/// snapshotting the event sink.
pub fn spawn_ingest(state: &ProxyState, ctx: &RequestContext, parsed: &Value) {
    let (Some(mcp_method), Some(method_str)) = (&ctx.mcp_method, &ctx.mcp_method_str) else {
        return;
    };
    if !proto_schema::is_schema_method(mcp_method) {
        return;
    }

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

    let method_owned = method_str.clone();
    let response_owned = parsed.clone();
    let bus: EventBus = state.event_bus.clone();
    let upstream_id = state.name.clone();
    let upstream_url = state.mcp_upstream.clone();

    state
        .schema_manager
        .spawn_ingest(method_owned, req_val, response_owned, move |version| {
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
