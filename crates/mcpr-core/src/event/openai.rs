//! Detection of ChatGPT Apps SDK metadata on JSON-RPC requests.
//!
//! ChatGPT (Apps SDK / Connectors path) injects an `openai/...` namespaced
//! extension into the standard MCP `_meta` field of `tools/call` requests.
//! Per OpenAI's docs, every value here is a hint for observability and
//! grouping; none is usable for authorization.
//!
//! Spec: <https://developers.openai.com/apps-sdk/reference>
//!
//! `_meta` shape on the wire (subset relevant to this module):
//! ```json
//! {
//!   "openai/locale": "en-US",
//!   "openai/userAgent": "Mozilla/5.0 ...",
//!   "openai/userLocation": { "city": "...", "country": "VN", ... },
//!   "openai/session": "v1/...",
//!   "openai/subject": "v1/...",
//!   "openai/organization": "v1/..."
//! }
//! ```

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::protocol::Request;
use crate::protocol::mcp::JsonRpcRequest;

/// ChatGPT Apps SDK metadata pulled from a JSON-RPC request's `params._meta`.
///
/// Each field is `Option` so partial payloads (e.g. anonymous chats with no
/// `openai/organization`) round-trip cleanly. `user_location` stays opaque
/// because OpenAI has not frozen the shape (lat/lng arrive as strings).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenAiClientContext {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub organization_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub locale: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_agent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_location: Option<Value>,
}

impl OpenAiClientContext {
    /// Detect `openai/*` keys in `rpc.params._meta`. Returns `None` when
    /// no such key is present, so non-ChatGPT clients add no overhead.
    pub fn from_jsonrpc_request(rpc: &JsonRpcRequest) -> Option<Self> {
        let meta = rpc.params.as_ref()?.get("_meta")?.as_object()?;

        let session_id = meta.get("openai/session").and_then(string_field);
        let subject_id = meta.get("openai/subject").and_then(string_field);
        let organization_id = meta.get("openai/organization").and_then(string_field);
        let locale = meta.get("openai/locale").and_then(string_field);
        let user_agent = meta.get("openai/userAgent").and_then(string_field);
        let user_location = meta.get("openai/userLocation").cloned();

        if session_id.is_none()
            && subject_id.is_none()
            && organization_id.is_none()
            && locale.is_none()
            && user_agent.is_none()
            && user_location.is_none()
        {
            return None;
        }

        Some(Self {
            session_id,
            subject_id,
            organization_id,
            locale,
            user_agent,
            user_location,
        })
    }

    /// Detect from any inbound `Request`. Batch falls back to the first
    /// rpc (the conversation-level metadata is shared across the batch),
    /// matching the existing `request_id` convention. HTTP traffic carries
    /// no JSON-RPC envelope so always returns `None`.
    pub fn from_request(req: &Request) -> Option<Self> {
        match req {
            Request::Mcp(_, rpc) => Self::from_jsonrpc_request(rpc),
            Request::McpBatch(_, rpcs) => rpcs.first().and_then(Self::from_jsonrpc_request),
            Request::Http(_) => None,
        }
    }
}

fn string_field(v: &Value) -> Option<String> {
    v.as_str().map(str::to_owned)
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;

    use bytes::Bytes;
    use serde_json::{Map, json};

    use crate::protocol::mcp::{ClientMethod, JsonRpcVersion, RequestId, ToolsMethod};

    fn rpc_with_params(params: Option<Map<String, Value>>) -> JsonRpcRequest {
        JsonRpcRequest {
            jsonrpc: JsonRpcVersion,
            id: RequestId::Number(1),
            method: ClientMethod::Tools(ToolsMethod::Call),
            params,
        }
    }

    fn rpc_with_meta(meta: Value) -> JsonRpcRequest {
        let mut params = Map::new();
        params.insert("_meta".into(), meta);
        rpc_with_params(Some(params))
    }

    fn full_meta() -> Value {
        json!({
            "openai/locale": "en-US",
            "openai/organization": "v1/org",
            "openai/session": "v1/sess",
            "openai/subject": "v1/subj",
            "openai/userAgent": "Mozilla/5.0",
            "openai/userLocation": {
                "city": "Vũng Tàu",
                "country": "VN",
                "latitude": "10.34599",
                "longitude": "107.08426",
                "timezone": "Asia/Ho_Chi_Minh"
            }
        })
    }

    // ── happy path ────────────────────────────────────────────────

    #[test]
    fn from_jsonrpc_request__pulls_all_six_openai_keys() {
        let ctx = OpenAiClientContext::from_jsonrpc_request(&rpc_with_meta(full_meta())).unwrap();

        assert_eq!(ctx.session_id.as_deref(), Some("v1/sess"));
        assert_eq!(ctx.subject_id.as_deref(), Some("v1/subj"));
        assert_eq!(ctx.organization_id.as_deref(), Some("v1/org"));
        assert_eq!(ctx.locale.as_deref(), Some("en-US"));
        assert_eq!(ctx.user_agent.as_deref(), Some("Mozilla/5.0"));
        assert_eq!(
            ctx.user_location.as_ref().unwrap().get("country").unwrap(),
            "VN"
        );
    }

    #[test]
    fn from_jsonrpc_request__partial_meta_only_fills_present_fields() {
        let ctx = OpenAiClientContext::from_jsonrpc_request(&rpc_with_meta(json!({
            "openai/session": "v1/only-session"
        })))
        .unwrap();

        assert_eq!(ctx.session_id.as_deref(), Some("v1/only-session"));
        assert!(ctx.subject_id.is_none());
        assert!(ctx.organization_id.is_none());
        assert!(ctx.locale.is_none());
        assert!(ctx.user_agent.is_none());
        assert!(ctx.user_location.is_none());
    }

    #[test]
    fn from_jsonrpc_request__user_location_kept_as_opaque_value() {
        let ctx = OpenAiClientContext::from_jsonrpc_request(&rpc_with_meta(json!({
            "openai/userLocation": { "country": "VN", "latitude": "10.3" }
        })))
        .unwrap();

        let loc = ctx.user_location.unwrap();
        assert_eq!(loc.get("country").unwrap(), "VN");
        assert_eq!(loc.get("latitude").unwrap(), "10.3");
    }

    // ── absence cases ─────────────────────────────────────────────

    #[test]
    fn from_jsonrpc_request__no_params_returns_none() {
        assert!(OpenAiClientContext::from_jsonrpc_request(&rpc_with_params(None)).is_none());
    }

    #[test]
    fn from_jsonrpc_request__no_meta_returns_none() {
        let mut params = Map::new();
        params.insert("name".into(), json!("search"));
        assert!(
            OpenAiClientContext::from_jsonrpc_request(&rpc_with_params(Some(params))).is_none()
        );
    }

    #[test]
    fn from_jsonrpc_request__non_object_meta_returns_none() {
        assert!(
            OpenAiClientContext::from_jsonrpc_request(&rpc_with_meta(json!("not-an-object")))
                .is_none()
        );
    }

    #[test]
    fn from_jsonrpc_request__meta_without_openai_keys_returns_none() {
        let ctx = OpenAiClientContext::from_jsonrpc_request(&rpc_with_meta(json!({
            "anthropic/session": "abc",
            "custom-key": 1
        })));
        assert!(ctx.is_none());
    }

    // ── type tolerance ────────────────────────────────────────────

    #[test]
    fn from_jsonrpc_request__non_string_id_fields_are_dropped() {
        // openai/session as a number rather than string: drop it. user_location
        // is the only field allowed to be a non-string, so it stays.
        let ctx = OpenAiClientContext::from_jsonrpc_request(&rpc_with_meta(json!({
            "openai/session": 12345,
            "openai/userLocation": { "country": "VN" }
        })))
        .unwrap();

        assert!(ctx.session_id.is_none());
        assert!(ctx.user_location.is_some());
    }

    // ── serde round-trip ──────────────────────────────────────────

    #[test]
    fn serde__omits_none_fields() {
        let ctx = OpenAiClientContext {
            session_id: Some("v1/s".into()),
            ..Default::default()
        };
        let v = serde_json::to_value(&ctx).unwrap();
        assert_eq!(v, json!({ "session_id": "v1/s" }));
    }

    #[test]
    fn serde__roundtrip_preserves_all_fields() {
        let ctx = OpenAiClientContext::from_jsonrpc_request(&rpc_with_meta(full_meta())).unwrap();
        let v = serde_json::to_value(&ctx).unwrap();
        let back: OpenAiClientContext = serde_json::from_value(v).unwrap();
        assert_eq!(ctx, back);
    }

    // ── from_request: variant dispatch ────────────────────────────

    fn req_parts() -> axum::http::request::Parts {
        axum::http::Request::builder()
            .method("POST")
            .uri("/")
            .body(())
            .unwrap()
            .into_parts()
            .0
    }

    #[test]
    fn from_request__mcp_delegates_to_jsonrpc_parser() {
        let req = Request::Mcp(req_parts(), rpc_with_meta(full_meta()));
        let ctx = OpenAiClientContext::from_request(&req).unwrap();
        assert_eq!(ctx.session_id.as_deref(), Some("v1/sess"));
    }

    #[test]
    fn from_request__batch_uses_first_rpc() {
        let req = Request::McpBatch(
            req_parts(),
            vec![
                rpc_with_meta(json!({ "openai/session": "v1/first" })),
                rpc_with_meta(json!({ "openai/session": "v1/second" })),
            ],
        );
        let ctx = OpenAiClientContext::from_request(&req).unwrap();
        assert_eq!(ctx.session_id.as_deref(), Some("v1/first"));
    }

    #[test]
    fn from_request__empty_batch_returns_none() {
        let req = Request::McpBatch(req_parts(), vec![]);
        assert!(OpenAiClientContext::from_request(&req).is_none());
    }

    #[test]
    fn from_request__http_returns_none() {
        let http_req = axum::http::Request::builder()
            .method("GET")
            .uri("/health")
            .body(Bytes::new())
            .unwrap();
        assert!(OpenAiClientContext::from_request(&Request::Http(http_req)).is_none());
    }
}
