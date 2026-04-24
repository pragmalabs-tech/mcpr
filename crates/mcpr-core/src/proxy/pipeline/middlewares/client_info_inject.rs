//! Request-side middleware: decode `clientInfo` from a client
//! `initialize` request and stash it on `Working`.
//!
//! Today's pipeline does this inline in `parser.rs:45-51`. In the new
//! shape, intake builds the shallow envelope and this middleware opts
//! into the typed `InitializeParams` view via `params_as::<Value>()`.
//! `SessionRecordMiddleware` later reads `cx.working.client` to emit a
//! populated `SessionStart`.

use async_trait::async_trait;

use crate::proxy::pipeline::message::{ClientKind, ClientMethod, LifecycleMethod};
use crate::proxy::pipeline::middleware::{Flow, RequestMiddleware};
use crate::proxy::pipeline::values::{Context, Request};

pub struct ClientInfoInjectMiddleware;

#[async_trait]
impl RequestMiddleware for ClientInfoInjectMiddleware {
    fn name(&self) -> &'static str {
        "client_info_inject"
    }

    async fn on_request(&self, req: Request, cx: &mut Context) -> Flow {
        let Request::Mcp(ref mcp) = req else {
            return Flow::Continue(req);
        };
        if !matches!(
            mcp.kind,
            ClientKind::Request(ClientMethod::Lifecycle(LifecycleMethod::Initialize))
        ) {
            return Flow::Continue(req);
        }
        if let Some(params) = mcp.envelope.params_as::<serde_json::Value>() {
            cx.working.client = crate::protocol::session::parse_client_info(&params);
        }
        Flow::Continue(req)
    }
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;

    use serde_json::json;

    use crate::proxy::pipeline::middlewares::test_support::{
        mcp_request, test_context, test_proxy_state,
    };

    #[tokio::test]
    async fn on_request__initialize_populates_client() {
        let proxy = test_proxy_state();
        let mut cx = test_context(proxy);
        let req = mcp_request(
            "initialize",
            json!({"clientInfo": {"name": "Claude Code", "version": "1.2.0"}}),
            None,
        );

        ClientInfoInjectMiddleware.on_request(req, &mut cx).await;
        let info = cx.working.client.expect("client info");
        assert_eq!(info.name, "Claude Code");
        assert_eq!(info.version.as_deref(), Some("1.2.0"));
    }

    #[tokio::test]
    async fn on_request__initialize_without_clientinfo_leaves_none() {
        let proxy = test_proxy_state();
        let mut cx = test_context(proxy);
        let req = mcp_request("initialize", json!({"capabilities": {}}), None);

        ClientInfoInjectMiddleware.on_request(req, &mut cx).await;
        assert!(cx.working.client.is_none());
    }

    #[tokio::test]
    async fn on_request__non_initialize_is_noop() {
        let proxy = test_proxy_state();
        let mut cx = test_context(proxy);
        let req = mcp_request("tools/list", serde_json::Value::Null, None);

        ClientInfoInjectMiddleware.on_request(req, &mut cx).await;
        assert!(cx.working.client.is_none());
    }

    #[tokio::test]
    async fn on_request__version_absent_still_populates_name() {
        let proxy = test_proxy_state();
        let mut cx = test_context(proxy);
        let req = mcp_request(
            "initialize",
            json!({"clientInfo": {"name": "cursor"}}),
            None,
        );

        ClientInfoInjectMiddleware.on_request(req, &mut cx).await;
        let info = cx.working.client.expect("client info");
        assert_eq!(info.name, "cursor");
        assert!(info.version.is_none());
    }
}
