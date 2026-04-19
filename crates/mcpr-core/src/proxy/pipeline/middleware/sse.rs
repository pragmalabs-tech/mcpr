//! Decode → mutate → encode bracket around the response middleware chain.
//!
//! [`DecodeResponseJson`] runs first: it parses the upstream body into
//! [`ResponseContext::json`] (unwrapping SSE framing transparently if
//! present). It does **not** touch `resp.body`. [`EncodeResponseJson`]
//! runs last: it re-serializes `resp.json` back into `resp.body` **only
//! when a prior middleware mutated it** (see
//! [`ResponseContext::json_mutated`]). Otherwise the original upstream
//! bytes pass through unchanged — preserving SSE `id:` / `retry:` lines,
//! JSON key ordering, and any server-side framing the proxy shouldn't be
//! rewriting.
//!
//! The names describe the action (decode/encode JSON), not a position in
//! the chain. SSE is incidental — it's just one encoding the pair handles.

use crate::protocol as jsonrpc;
use crate::proxy::sse::{extract_json_from_sse, wrap_as_sse};
use async_trait::async_trait;
use serde_json::Value;

use super::ResponseMiddleware;
use crate::proxy::pipeline::context::{RequestContext, ResponseContext};
use crate::proxy::proxy_state::ProxyState;

/// Parse the raw response body into `resp.json`. If the body was
/// SSE-wrapped, unwrap it for the JSON parse and set `resp.was_sse = true`
/// so `EncodeResponseJson` knows to re-wrap if the JSON ends up mutated.
/// Also extracts any JSON-RPC error into `resp.rpc_error` so later
/// middleware / emit can see it.
///
/// Does **not** touch `resp.body` — the original upstream bytes stay intact.
pub struct DecodeResponseJson;

#[async_trait]
impl ResponseMiddleware for DecodeResponseJson {
    async fn on_response(
        &self,
        _state: &ProxyState,
        _req: &RequestContext,
        resp: &mut ResponseContext,
    ) {
        // Parse from a borrowed slice — no body clone on the non-SSE path.
        let extracted = extract_json_from_sse(&resp.body);
        let (json_slice, is_sse): (&[u8], bool) = match &extracted {
            Some(v) => (v.as_slice(), true),
            None => (resp.body.as_slice(), false),
        };
        resp.was_sse = is_sse;

        if let Ok(value) = serde_json::from_slice::<Value>(json_slice) {
            resp.rpc_error =
                jsonrpc::extract_error_code(&value).map(|(code, msg)| (code, msg.to_string()));
            resp.json = Some(value);
        }
    }
}

#[cfg(test)]
#[allow(non_snake_case)]
mod decode_tests {
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
    async fn decode__extracts_rpc_error_from_json_body() {
        let body = serde_json::to_vec(&json!({
            "jsonrpc": "2.0", "id": 1,
            "error": {"code": -32601, "message": "method not found"}
        }))
        .unwrap();
        let state = test_state();
        let req = empty_req();
        let mut resp = ResponseContext::new(200, HeaderMap::new(), body, None);
        DecodeResponseJson
            .on_response(&state, &req, &mut resp)
            .await;
        assert!(!resp.was_sse);
        assert!(
            !resp.json_mutated,
            "decode itself does not count as mutation"
        );
        assert_eq!(
            resp.rpc_error.as_ref().map(|(c, m)| (*c, m.as_str())),
            Some((-32601, "method not found"))
        );
    }

    #[tokio::test]
    async fn decode__leaves_body_untouched() {
        let original = br#"data: {"jsonrpc":"2.0","id":1,"result":{}}

"#
        .to_vec();
        let mut resp = ResponseContext::new(200, HeaderMap::new(), original.clone(), None);
        DecodeResponseJson
            .on_response(&test_state(), &empty_req(), &mut resp)
            .await;
        assert!(resp.was_sse);
        assert_eq!(
            resp.body, original,
            "decode must not mutate body — byte-pass path depends on it"
        );
    }
}

/// Re-serialize `resp.json` back into `resp.body`. **No-op unless a prior
/// middleware set `resp.json_mutated = true`**. When skipping, the
/// original upstream body passes through unchanged (SSE framing metadata
/// preserved, byte-for-byte identity with upstream).
///
/// If `was_sse` is set, the re-serialized JSON is wrapped in a fresh
/// `data: …\n\n` frame. Note that re-wrapping **does** lose SSE metadata
/// (`id:` / `retry:` / multi-event framing) from the original upstream —
/// this is an inherent limitation of parse-then-reserialize and only
/// matters when a middleware actually needed to mutate.
pub struct EncodeResponseJson;

#[async_trait]
impl ResponseMiddleware for EncodeResponseJson {
    async fn on_response(
        &self,
        _state: &ProxyState,
        _req: &RequestContext,
        resp: &mut ResponseContext,
    ) {
        if !resp.json_mutated {
            // Byte-pass fast path — body already holds the correct bytes.
            return;
        }
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

#[cfg(test)]
#[allow(non_snake_case)]
mod encode_tests {
    use axum::http::HeaderMap;
    use serde_json::json;

    use super::*;
    use crate::proxy::pipeline::context::ResponseContext;

    // Reuse the test harness from decode_tests.
    use super::decode_tests::{empty_req, test_state};

    #[tokio::test]
    async fn encode__no_op_when_json_not_mutated() {
        let original = br#"{"key":"original"}"#.to_vec();
        let mut resp = ResponseContext::new(200, HeaderMap::new(), original.clone(), None);
        resp.json = Some(json!({ "key": "doesn't matter, flag not set" }));
        resp.json_mutated = false;

        EncodeResponseJson
            .on_response(&test_state(), &empty_req(), &mut resp)
            .await;

        assert_eq!(
            resp.body, original,
            "encode must not touch body when json_mutated is false"
        );
    }

    #[tokio::test]
    async fn encode__serializes_when_mutated() {
        let mut resp = ResponseContext::new(200, HeaderMap::new(), b"stale".to_vec(), None);
        resp.json = Some(json!({ "fresh": "yes" }));
        resp.json_mutated = true;

        EncodeResponseJson
            .on_response(&test_state(), &empty_req(), &mut resp)
            .await;

        let parsed: serde_json::Value = serde_json::from_slice(&resp.body).unwrap();
        assert_eq!(parsed["fresh"], "yes");
    }

    #[tokio::test]
    async fn encode__wraps_sse_when_mutated_and_was_sse() {
        let mut resp = ResponseContext::new(200, HeaderMap::new(), b"stale".to_vec(), None);
        resp.json = Some(json!({ "k": "v" }));
        resp.json_mutated = true;
        resp.was_sse = true;

        EncodeResponseJson
            .on_response(&test_state(), &empty_req(), &mut resp)
            .await;

        assert!(resp.body.starts_with(b"data: "));
        assert!(resp.body.ends_with(b"\n\n"));
    }
}
