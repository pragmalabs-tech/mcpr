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
