//! Request-side middleware: extract the per-method target identifier
//! from JSON-RPC `params` and stash it on `Working`.
//!
//! Each MCP request kind that operates on a named entity carries an
//! identifier in `params`. Capturing them here lets `emit` populate
//! `RequestEvent` so downstream consumers (SQLite log, cloud dashboard)
//! can group, filter, and surface per-target metrics:
//!
//! | Method                  | Identifier in `params` | Stashed on `Working` as |
//! |-------------------------|------------------------|--------------------------|
//! | `tools/call`            | `name`                 | `request_tool`           |
//! | `resources/read`        | `uri`                  | `request_resource_uri`   |
//! | `resources/subscribe`   | `uri`                  | `request_resource_uri`   |
//! | `resources/unsubscribe` | `uri`                  | `request_resource_uri`   |
//! | `prompts/get`           | `name`                 | `request_prompt_name`    |
//!
//! Methods without a useful identifier (`tools/list`, `resources/list`,
//! `initialize`, …) are no-ops.

use async_trait::async_trait;
use serde::Deserialize;

use crate::protocol::mcp::{ClientKind, ClientMethod, PromptsMethod, ResourcesMethod, ToolsMethod};
use crate::proxy::pipeline::middleware::{Flow, RequestMiddleware};
use crate::proxy::pipeline::values::{Context, Request};

pub struct TargetExtractMiddleware;

#[derive(Deserialize)]
struct NameParams {
    name: String,
}

#[derive(Deserialize)]
struct UriParams {
    uri: String,
}

#[async_trait]
impl RequestMiddleware for TargetExtractMiddleware {
    fn name(&self) -> &'static str {
        "target_extract"
    }

    async fn on_request(&self, req: Request, cx: &mut Context) -> Flow {
        let Request::Mcp(ref mcp) = req else {
            return Flow::Continue(req);
        };
        let ClientKind::Request(method) = &mcp.kind else {
            return Flow::Continue(req);
        };

        match method {
            ClientMethod::Tools(ToolsMethod::Call) => {
                if let Some(p) = mcp.envelope.params_as::<NameParams>() {
                    cx.working.request_tool = Some(p.name);
                }
            }
            ClientMethod::Resources(
                ResourcesMethod::Read | ResourcesMethod::Subscribe | ResourcesMethod::Unsubscribe,
            ) => {
                if let Some(p) = mcp.envelope.params_as::<UriParams>() {
                    cx.working.request_resource_uri = Some(p.uri);
                }
            }
            ClientMethod::Prompts(PromptsMethod::Get) => {
                if let Some(p) = mcp.envelope.params_as::<NameParams>() {
                    cx.working.request_prompt_name = Some(p.name);
                }
            }
            _ => {}
        }

        Flow::Continue(req)
    }
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;

    use axum::body::Body;
    use axum::http::{HeaderMap, Method};
    use serde_json::json;

    use crate::proxy::pipeline::middlewares::test_support::{
        mcp_request, test_context, test_proxy_state,
    };
    use crate::proxy::pipeline::values::RawRequest;

    // ── tools/call → request_tool ──────────────────────────────

    #[tokio::test]
    async fn on_request__tools_call_stashes_tool_name() {
        let proxy = test_proxy_state();
        let mut cx = test_context(proxy);
        let req = mcp_request(
            "tools/call",
            json!({"name": "weather", "arguments": {"city": "Paris"}}),
            None,
        );

        TargetExtractMiddleware.on_request(req, &mut cx).await;
        assert_eq!(cx.working.request_tool.as_deref(), Some("weather"));
    }

    #[tokio::test]
    async fn on_request__tools_call_missing_name_leaves_none() {
        let proxy = test_proxy_state();
        let mut cx = test_context(proxy);
        let req = mcp_request("tools/call", json!({"arguments": {"city": "Paris"}}), None);

        TargetExtractMiddleware.on_request(req, &mut cx).await;
        assert!(cx.working.request_tool.is_none());
    }

    #[tokio::test]
    async fn on_request__tools_call_empty_name_stashes_empty_string() {
        let proxy = test_proxy_state();
        let mut cx = test_context(proxy);
        let req = mcp_request("tools/call", json!({"name": ""}), None);

        TargetExtractMiddleware.on_request(req, &mut cx).await;
        assert_eq!(cx.working.request_tool.as_deref(), Some(""));
    }

    // ── resources/* → request_resource_uri ─────────────────────

    #[tokio::test]
    async fn on_request__resources_read_stashes_uri() {
        let proxy = test_proxy_state();
        let mut cx = test_context(proxy);
        let req = mcp_request("resources/read", json!({"uri": "file:///etc/hosts"}), None);

        TargetExtractMiddleware.on_request(req, &mut cx).await;
        assert_eq!(
            cx.working.request_resource_uri.as_deref(),
            Some("file:///etc/hosts")
        );
        assert!(cx.working.request_tool.is_none());
        assert!(cx.working.request_prompt_name.is_none());
    }

    #[tokio::test]
    async fn on_request__resources_subscribe_stashes_uri() {
        let proxy = test_proxy_state();
        let mut cx = test_context(proxy);
        let req = mcp_request("resources/subscribe", json!({"uri": "logs://stream"}), None);

        TargetExtractMiddleware.on_request(req, &mut cx).await;
        assert_eq!(
            cx.working.request_resource_uri.as_deref(),
            Some("logs://stream")
        );
    }

    #[tokio::test]
    async fn on_request__resources_unsubscribe_stashes_uri() {
        let proxy = test_proxy_state();
        let mut cx = test_context(proxy);
        let req = mcp_request(
            "resources/unsubscribe",
            json!({"uri": "logs://stream"}),
            None,
        );

        TargetExtractMiddleware.on_request(req, &mut cx).await;
        assert_eq!(
            cx.working.request_resource_uri.as_deref(),
            Some("logs://stream")
        );
    }

    #[tokio::test]
    async fn on_request__resources_read_missing_uri_leaves_none() {
        let proxy = test_proxy_state();
        let mut cx = test_context(proxy);
        let req = mcp_request("resources/read", json!({}), None);

        TargetExtractMiddleware.on_request(req, &mut cx).await;
        assert!(cx.working.request_resource_uri.is_none());
    }

    // ── prompts/get → request_prompt_name ──────────────────────

    #[tokio::test]
    async fn on_request__prompts_get_stashes_prompt_name() {
        let proxy = test_proxy_state();
        let mut cx = test_context(proxy);
        let req = mcp_request(
            "prompts/get",
            json!({"name": "code_review", "arguments": {}}),
            None,
        );

        TargetExtractMiddleware.on_request(req, &mut cx).await;
        assert_eq!(
            cx.working.request_prompt_name.as_deref(),
            Some("code_review")
        );
        assert!(cx.working.request_tool.is_none());
        assert!(cx.working.request_resource_uri.is_none());
    }

    // ── methods without identifiers ────────────────────────────

    #[tokio::test]
    async fn on_request__tools_list_is_noop() {
        let proxy = test_proxy_state();
        let mut cx = test_context(proxy);
        let req = mcp_request("tools/list", serde_json::Value::Null, None);

        TargetExtractMiddleware.on_request(req, &mut cx).await;
        assert!(cx.working.request_tool.is_none());
        assert!(cx.working.request_resource_uri.is_none());
        assert!(cx.working.request_prompt_name.is_none());
    }

    #[tokio::test]
    async fn on_request__resources_list_is_noop() {
        let proxy = test_proxy_state();
        let mut cx = test_context(proxy);
        let req = mcp_request("resources/list", serde_json::Value::Null, None);

        TargetExtractMiddleware.on_request(req, &mut cx).await;
        assert!(cx.working.request_resource_uri.is_none());
    }

    #[tokio::test]
    async fn on_request__initialize_is_noop() {
        let proxy = test_proxy_state();
        let mut cx = test_context(proxy);
        let req = mcp_request("initialize", json!({"protocolVersion": "2025-11-25"}), None);

        TargetExtractMiddleware.on_request(req, &mut cx).await;
        assert!(cx.working.request_tool.is_none());
        assert!(cx.working.request_resource_uri.is_none());
        assert!(cx.working.request_prompt_name.is_none());
    }

    #[tokio::test]
    async fn on_request__non_mcp_passthrough() {
        let proxy = test_proxy_state();
        let mut cx = test_context(proxy);
        let req = Request::Raw(RawRequest {
            method: Method::GET,
            path: "/health".into(),
            body: Body::empty(),
            headers: HeaderMap::new(),
        });

        TargetExtractMiddleware.on_request(req, &mut cx).await;
        assert!(cx.working.request_tool.is_none());
        assert!(cx.working.request_resource_uri.is_none());
        assert!(cx.working.request_prompt_name.is_none());
    }
}
