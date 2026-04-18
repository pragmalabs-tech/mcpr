//! SSE unwrap/wrap middleware — parses `event-stream` wrapped JSON into
//! `resp.json` on the way in, and re-wraps the serialized body on the way out.

use crate::protocol as jsonrpc;
use crate::proxy::sse::{extract_json_from_sse, wrap_as_sse};
use async_trait::async_trait;
use serde_json::Value;

use super::ResponseMiddleware;
use crate::proxy::pipeline::context::{RequestContext, ResponseContext};
use crate::proxy::proxy_state::ProxyState;

/// Parse the raw response body into `resp.json`. If the body was SSE-wrapped,
/// unwrap it first and set `resp.was_sse = true` so `SseWrapMiddleware` knows to
/// re-wrap after mutations. Also extracts any JSON-RPC error into
/// `resp.rpc_error` so later stages / emit can see it.
pub struct SseUnwrapMiddleware;

#[async_trait]
impl ResponseMiddleware for SseUnwrapMiddleware {
    async fn on_response(
        &self,
        _state: &ProxyState,
        _req: &RequestContext,
        resp: &mut ResponseContext,
    ) {
        let (json_bytes, is_sse) = match extract_json_from_sse(&resp.body) {
            Some(extracted) => (extracted, true),
            None => (resp.body.clone(), false),
        };
        resp.was_sse = is_sse;

        if let Ok(value) = serde_json::from_slice::<Value>(&json_bytes) {
            resp.rpc_error =
                jsonrpc::extract_error_code(&value).map(|(code, msg)| (code, msg.to_string()));
            resp.json = Some(value);
        }
    }
}

#[cfg(test)]
#[allow(non_snake_case)]
mod sse_unwrap_tests {
    use axum::http::HeaderMap;
    use serde_json::json;
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    use tokio::sync::RwLock;

    use super::*;
    use crate::protocol::schema_manager::{MemorySchemaStore, SchemaManager};
    use crate::protocol::session::MemorySessionStore;
    use crate::proxy::forwarding::UpstreamClient;
    use crate::proxy::{CspConfig, RewriteConfig, new_shared_health};

    fn test_state() -> ProxyState {
        ProxyState {
            name: "t".into(),
            mcp_upstream: "http://u".into(),
            upstream: UpstreamClient {
                http_client: reqwest::Client::builder().build().unwrap(),
                semaphore: Arc::new(tokio::sync::Semaphore::new(1)),
                request_timeout: Duration::from_secs(1),
            },
            max_request_body: 1024,
            max_response_body: 1024,
            rewrite_config: Arc::new(RwLock::new(RewriteConfig {
                proxy_url: "http://p".into(),
                proxy_domain: "p".into(),
                mcp_upstream: "http://u".into(),
                csp: CspConfig::default(),
            })),
            widget_source: None,
            sessions: MemorySessionStore::new(),
            schema_manager: Arc::new(SchemaManager::new("t", MemorySchemaStore::new())),
            health: new_shared_health(),
            event_bus: crate::event::EventManager::new().start().bus,
        }
    }

    fn empty_req() -> RequestContext {
        RequestContext {
            start: Instant::now(),
            http_method: axum::http::Method::POST,
            path: "/mcp".into(),
            request_size: 0,
            wants_sse: false,
            session_id: None,
            jsonrpc: None,
            mcp_method: None,
            mcp_method_str: None,
            tool: None,
            is_batch: false,
            client_info_from_init: None,
            client_name: None,
            client_version: None,
            tags: Vec::new(),
        }
    }

    #[tokio::test]
    async fn sse_unwrap__extracts_rpc_error_from_json_body() {
        let body = serde_json::to_vec(&json!({
            "jsonrpc": "2.0", "id": 1,
            "error": {"code": -32601, "message": "method not found"}
        }))
        .unwrap();
        let state = test_state();
        let req = empty_req();
        let mut resp = ResponseContext::new(200, HeaderMap::new(), body, None);
        SseUnwrapMiddleware
            .on_response(&state, &req, &mut resp)
            .await;
        assert!(!resp.was_sse);
        assert_eq!(
            resp.rpc_error.as_ref().map(|(c, m)| (*c, m.as_str())),
            Some((-32601, "method not found"))
        );
    }
}

/// Re-serialize `resp.json` back into `resp.body`. If `resp.was_sse` is set,
/// wrap the serialized body in SSE `data:` framing. No-op when `resp.json`
/// is `None` (non-JSON response — left untouched).
pub struct SseWrapMiddleware;

#[async_trait]
impl ResponseMiddleware for SseWrapMiddleware {
    async fn on_response(
        &self,
        _state: &ProxyState,
        _req: &RequestContext,
        resp: &mut ResponseContext,
    ) {
        let Some(json) = &resp.json else {
            return;
        };
        let serialized = serde_json::to_vec(json).unwrap_or_else(|_| resp.body.clone());
        resp.body = if resp.was_sse {
            wrap_as_sse(&serialized)
        } else {
            serialized
        };
    }
}
