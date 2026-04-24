//! Response-side middleware: mark schema stale on server-initiated
//! `list_changed` notifications. Pattern-matches on `ServerNotifMethod`
//! directly — no JSON tree walk.
//!
//! Known limitation: fires only on `Response::McpBuffered`.
//! `list_changed` notifications that arrive mid-stream inside
//! `McpStreamed` bodies stay unobserved until server-push observability
//! lands.

use async_trait::async_trait;

use crate::protocol;
use crate::proxy::pipeline::message::{MessageKind, ServerKind, ServerNotifMethod};
use crate::proxy::pipeline::middleware::ResponseMiddleware;
use crate::proxy::pipeline::values::{Context, Response};

pub struct SchemaStaleMiddleware;

#[async_trait]
impl ResponseMiddleware for SchemaStaleMiddleware {
    fn name(&self) -> &'static str {
        "schema_stale"
    }

    async fn on_response(&self, resp: Response, cx: &mut Context) -> Response {
        let message = match &resp {
            Response::McpBuffered { message, .. } => message,
            _ => return resp,
        };
        let MessageKind::Server(ServerKind::Notification(n)) = &message.kind else {
            return resp;
        };
        let method = match n {
            ServerNotifMethod::ToolsListChanged => protocol::TOOLS_LIST,
            ServerNotifMethod::ResourcesListChanged => protocol::RESOURCES_LIST,
            ServerNotifMethod::PromptsListChanged => protocol::PROMPTS_LIST,
            _ => return resp,
        };
        cx.intake.proxy.schema_manager.mark_stale(method);
        resp
    }
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;

    use axum::http::StatusCode;

    use crate::proxy::pipeline::middlewares::test_support::{
        mcp_buffered_response, test_context, test_proxy_state,
    };

    #[tokio::test]
    async fn on_response__tools_list_changed_marks_stale() {
        let proxy = test_proxy_state();
        let mut cx = test_context(proxy.clone());
        let resp = mcp_buffered_response(
            r#"{"jsonrpc":"2.0","method":"notifications/tools/list_changed"}"#,
            StatusCode::OK,
        );

        SchemaStaleMiddleware.on_response(resp, &mut cx).await;
        assert!(proxy.schema_manager.is_stale("tools/list"));
    }

    #[tokio::test]
    async fn on_response__resources_list_changed_marks_stale() {
        let proxy = test_proxy_state();
        let mut cx = test_context(proxy.clone());
        let resp = mcp_buffered_response(
            r#"{"jsonrpc":"2.0","method":"notifications/resources/list_changed"}"#,
            StatusCode::OK,
        );

        SchemaStaleMiddleware.on_response(resp, &mut cx).await;
        assert!(proxy.schema_manager.is_stale("resources/list"));
    }

    #[tokio::test]
    async fn on_response__prompts_list_changed_marks_stale() {
        let proxy = test_proxy_state();
        let mut cx = test_context(proxy.clone());
        let resp = mcp_buffered_response(
            r#"{"jsonrpc":"2.0","method":"notifications/prompts/list_changed"}"#,
            StatusCode::OK,
        );

        SchemaStaleMiddleware.on_response(resp, &mut cx).await;
        assert!(proxy.schema_manager.is_stale("prompts/list"));
    }

    #[tokio::test]
    async fn on_response__unrelated_notification_passthrough() {
        let proxy = test_proxy_state();
        let mut cx = test_context(proxy.clone());
        let resp = mcp_buffered_response(
            r#"{"jsonrpc":"2.0","method":"notifications/progress","params":{"progressToken":"x","progress":1}}"#,
            StatusCode::OK,
        );

        SchemaStaleMiddleware.on_response(resp, &mut cx).await;
        assert!(!proxy.schema_manager.is_stale("tools/list"));
    }

    #[tokio::test]
    async fn on_response__result_passthrough() {
        let proxy = test_proxy_state();
        let mut cx = test_context(proxy.clone());
        let resp = mcp_buffered_response(
            r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[]}}"#,
            StatusCode::OK,
        );

        SchemaStaleMiddleware.on_response(resp, &mut cx).await;
        assert!(!proxy.schema_manager.is_stale("tools/list"));
    }

    #[tokio::test]
    async fn on_response__non_buffered_passthrough() {
        let proxy = test_proxy_state();
        let mut cx = test_context(proxy.clone());
        let resp = Response::Upstream502 {
            reason: "boom".into(),
        };

        SchemaStaleMiddleware.on_response(resp, &mut cx).await;
        assert!(!proxy.schema_manager.is_stale("tools/list"));
    }
}
