//! Request-side middleware: extract the tool name from `tools/call`
//! params and stash it on `Working`.
//!
//! `emit::build_request_event` reads `cx.working.request_tool` to
//! populate `RequestEvent.tool`. Without this stash every `tools/call`
//! event reaches the SQLite log and the cloud sink with an empty tool
//! field, breaking per-tool filtering and the dashboard's "Slow Calls"
//! view (which filters `WHERE tool != ''`).

use async_trait::async_trait;
use serde::Deserialize;

use crate::protocol::mcp::{ClientKind, ClientMethod, ToolsMethod};
use crate::proxy::pipeline::middleware::{Flow, RequestMiddleware};
use crate::proxy::pipeline::values::{Context, Request};

pub struct ToolExtractMiddleware;

#[derive(Deserialize)]
struct ToolsCallParams {
    name: String,
}

#[async_trait]
impl RequestMiddleware for ToolExtractMiddleware {
    fn name(&self) -> &'static str {
        "tool_extract"
    }

    async fn on_request(&self, req: Request, cx: &mut Context) -> Flow {
        let Request::Mcp(ref mcp) = req else {
            return Flow::Continue(req);
        };
        if !matches!(
            mcp.kind,
            ClientKind::Request(ClientMethod::Tools(ToolsMethod::Call))
        ) {
            return Flow::Continue(req);
        }
        if let Some(params) = mcp.envelope.params_as::<ToolsCallParams>() {
            cx.working.request_tool = Some(params.name);
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

    #[tokio::test]
    async fn on_request__tools_call_stashes_tool_name() {
        let proxy = test_proxy_state();
        let mut cx = test_context(proxy);
        let req = mcp_request(
            "tools/call",
            json!({"name": "weather", "arguments": {"city": "Paris"}}),
            None,
        );

        ToolExtractMiddleware.on_request(req, &mut cx).await;
        assert_eq!(cx.working.request_tool.as_deref(), Some("weather"));
    }

    #[tokio::test]
    async fn on_request__tools_call_missing_name_leaves_none() {
        let proxy = test_proxy_state();
        let mut cx = test_context(proxy);
        let req = mcp_request("tools/call", json!({"arguments": {"city": "Paris"}}), None);

        ToolExtractMiddleware.on_request(req, &mut cx).await;
        assert!(cx.working.request_tool.is_none());
    }

    #[tokio::test]
    async fn on_request__tools_call_empty_name_stashes_empty_string() {
        let proxy = test_proxy_state();
        let mut cx = test_context(proxy);
        let req = mcp_request("tools/call", json!({"name": ""}), None);

        ToolExtractMiddleware.on_request(req, &mut cx).await;
        assert_eq!(cx.working.request_tool.as_deref(), Some(""));
    }

    #[tokio::test]
    async fn on_request__tools_list_is_noop() {
        let proxy = test_proxy_state();
        let mut cx = test_context(proxy);
        let req = mcp_request("tools/list", serde_json::Value::Null, None);

        ToolExtractMiddleware.on_request(req, &mut cx).await;
        assert!(cx.working.request_tool.is_none());
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

        ToolExtractMiddleware.on_request(req, &mut cx).await;
        assert!(cx.working.request_tool.is_none());
    }
}
