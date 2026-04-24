//! Response-side middleware: rewrite widget CSP directives carried in
//! list / call / read results.
//!
//! Holds the same `ArcSwap<RewriteConfig>` handle that `ProxyState`
//! holds so `mcpr.toml` reloads swap the inner `Arc` without restarting
//! the middleware.
//!
//! Fast path: byte-scan the raw `result` bytes for CSP-shaped keys
//! (`connect_domains`, `openai/widgetCSP`, etc). Miss → no parse, no
//! allocation. Hit → deserialize, mutate via `rewrite_response`,
//! re-wrap into the message's `result` field.

use std::sync::Arc;

use arc_swap::ArcSwap;
use async_trait::async_trait;
use serde_json::Value;

use crate::proxy::pipeline::message::{ClientMethod, ResourcesMethod, ToolsMethod};
use crate::proxy::pipeline::middleware::ResponseMiddleware;
use crate::proxy::pipeline::values::{Context, Response};
use crate::proxy::{RewriteConfig, rewrite_response};

use super::shared;

/// CSP-shaped keys that `rewrite_response` can mutate. If none of these
/// appear as a substring in the `result` bytes, there is nothing to
/// rewrite — skip the parse.
const MARKERS: &[&[u8]] = &[
    b"connect_domains",
    b"resource_domains",
    b"frame_domains",
    b"connectDomains",
    b"resourceDomains",
    b"frameDomains",
    b"openai/widgetCSP",
    b"ui.csp",
    b"openai/widgetDomain",
];

pub struct CspRewriteMiddleware {
    config: Arc<ArcSwap<RewriteConfig>>,
}

impl CspRewriteMiddleware {
    pub fn new(config: Arc<ArcSwap<RewriteConfig>>) -> Self {
        Self { config }
    }
}

#[async_trait]
impl ResponseMiddleware for CspRewriteMiddleware {
    fn name(&self) -> &'static str {
        "csp_rewrite"
    }

    async fn on_response(&self, resp: Response, cx: &mut Context) -> Response {
        let Response::McpBuffered {
            envelope,
            mut message,
            status,
            headers,
        } = resp
        else {
            return resp;
        };

        let eligible = matches!(
            cx.working.request_method,
            Some(ClientMethod::Tools(ToolsMethod::List))
                | Some(ClientMethod::Tools(ToolsMethod::Call))
                | Some(ClientMethod::Resources(ResourcesMethod::Read))
        );
        let raw_bytes = message.envelope.result.as_ref().map(|r| r.get().as_bytes());
        let should_rewrite = eligible && raw_bytes.map(has_markers).unwrap_or(false);
        if !should_rewrite {
            return Response::McpBuffered {
                envelope,
                message,
                status,
                headers,
            };
        }

        let method_str = cx
            .working
            .request_method
            .as_ref()
            .and_then(shared::client_method_str)
            .unwrap_or("");
        let Ok(result_val) = serde_json::from_slice::<Value>(raw_bytes.unwrap()) else {
            return Response::McpBuffered {
                envelope,
                message,
                status,
                headers,
            };
        };

        let mut wrapper = Value::Object(Default::default());
        wrapper["result"] = result_val;
        let cfg = self.config.load();
        if rewrite_response(method_str, &mut wrapper, &cfg) {
            let rewritten = wrapper
                .get_mut("result")
                .map(std::mem::take)
                .unwrap_or(Value::Null);
            if let Ok(boxed) = serde_json::value::to_raw_value(&rewritten) {
                message.envelope.result = Some(boxed);
            }
        }

        Response::McpBuffered {
            envelope,
            message,
            status,
            headers,
        }
    }
}

fn has_markers(body: &[u8]) -> bool {
    MARKERS.iter().any(|m| contains_slice(body, m))
}

fn contains_slice(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || haystack.len() < needle.len() {
        return needle.is_empty();
    }
    haystack.windows(needle.len()).any(|win| win == needle)
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;

    use axum::http::StatusCode;

    use crate::proxy::CspConfig;
    use crate::proxy::pipeline::middlewares::test_support::{
        mcp_buffered_response, set_request_method, test_context, test_proxy_state,
    };

    fn middleware(proxy: &Arc<crate::proxy::ProxyState>) -> CspRewriteMiddleware {
        CspRewriteMiddleware::new(proxy.rewrite_config.clone())
    }

    fn extract_result(resp: &Response) -> Value {
        match resp {
            Response::McpBuffered { message, .. } => {
                message.envelope.result_as::<Value>().unwrap_or(Value::Null)
            }
            _ => panic!("expected McpBuffered"),
        }
    }

    #[tokio::test]
    async fn on_response__non_eligible_method_passthrough() {
        let proxy = test_proxy_state();
        let mut cx = test_context(proxy.clone());
        // No request_method stashed → ineligible.
        let resp = mcp_buffered_response(
            r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[{"_meta":{"openai/widgetCSP":{"connect_domains":["http://upstream.test"]}}}]}}"#,
            StatusCode::OK,
        );

        let out = middleware(&proxy).on_response(resp, &mut cx).await;
        let result = extract_result(&out);
        // Identity: connect_domains still points at upstream, unchanged.
        let connect = result["tools"][0]["_meta"]["openai/widgetCSP"]["connect_domains"]
            .as_array()
            .unwrap();
        assert_eq!(connect[0].as_str(), Some("http://upstream.test"));
    }

    #[tokio::test]
    async fn on_response__no_markers_passthrough_identity() {
        let proxy = test_proxy_state();
        let mut cx = test_context(proxy.clone());
        set_request_method(&mut cx, ClientMethod::Tools(ToolsMethod::List));
        let resp = mcp_buffered_response(
            r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[{"name":"one"}]}}"#,
            StatusCode::OK,
        );

        let out = middleware(&proxy).on_response(resp, &mut cx).await;
        let result = extract_result(&out);
        assert_eq!(result["tools"][0]["name"], "one");
    }

    #[tokio::test]
    async fn on_response__markers_trigger_rewrite_for_tools_list() {
        let proxy = test_proxy_state();
        let mut cx = test_context(proxy.clone());
        set_request_method(&mut cx, ClientMethod::Tools(ToolsMethod::List));
        let resp = mcp_buffered_response(
            r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[{"_meta":{"openai/widgetCSP":{"connect_domains":["http://upstream.test/api"]}}}]}}"#,
            StatusCode::OK,
        );

        let out = middleware(&proxy).on_response(resp, &mut cx).await;
        let result = extract_result(&out);
        let connect = result["tools"][0]["_meta"]["openai/widgetCSP"]["connect_domains"]
            .as_array()
            .unwrap();
        let rewritten = connect.iter().any(|v| {
            v.as_str()
                .map(|s| s.contains("proxy.test"))
                .unwrap_or(false)
        });
        assert!(
            rewritten,
            "expected upstream rewritten into proxy URL, got {connect:?}"
        );
    }

    #[tokio::test]
    async fn on_response__arc_swap_hot_reload_uses_new_config() {
        let proxy = test_proxy_state();
        let mut cx = test_context(proxy.clone());
        set_request_method(&mut cx, ClientMethod::Tools(ToolsMethod::List));

        // Swap to a config with a different proxy_url.
        proxy.rewrite_config.store(Arc::new(RewriteConfig {
            proxy_url: "https://proxy-v2.test".into(),
            proxy_domain: "proxy-v2.test".into(),
            mcp_upstream: "http://upstream.test".into(),
            csp: CspConfig::default(),
        }));

        let resp = mcp_buffered_response(
            r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[{"_meta":{"openai/widgetCSP":{"connect_domains":["http://upstream.test/api"]}}}]}}"#,
            StatusCode::OK,
        );

        let out = middleware(&proxy).on_response(resp, &mut cx).await;
        let result = extract_result(&out);
        let connect = result["tools"][0]["_meta"]["openai/widgetCSP"]["connect_domains"]
            .as_array()
            .unwrap();
        let seen_v2 = connect.iter().any(|v| {
            v.as_str()
                .map(|s| s.contains("proxy-v2.test"))
                .unwrap_or(false)
        });
        assert!(seen_v2, "expected v2 proxy host in rewritten output");
    }

    #[tokio::test]
    async fn on_response__non_buffered_passthrough() {
        let proxy = test_proxy_state();
        let mut cx = test_context(proxy.clone());
        set_request_method(&mut cx, ClientMethod::Tools(ToolsMethod::List));
        let resp = Response::Upstream502 { reason: "x".into() };

        let out = middleware(&proxy).on_response(resp, &mut cx).await;
        assert!(matches!(out, Response::Upstream502 { .. }));
    }

    #[test]
    fn has_markers__finds_snake_case() {
        assert!(has_markers(br#"{"connect_domains":["http://a"]}"#));
    }

    #[test]
    fn has_markers__finds_openai_shape() {
        assert!(has_markers(br#"{"openai/widgetCSP":{}}"#));
    }

    #[test]
    fn has_markers__plain_tool_call_no_markers() {
        assert!(!has_markers(
            br#"{"content":[{"type":"text","text":"hi"}]}"#
        ));
    }
}
