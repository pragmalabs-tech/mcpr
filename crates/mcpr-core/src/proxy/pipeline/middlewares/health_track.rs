//! Response-side middleware: flip the shared proxy-health flag to
//! "connected" on a successful `initialize` response.

use async_trait::async_trait;

use crate::proxy::lock_health;
use crate::proxy::pipeline::message::{ClientMethod, LifecycleMethod};
use crate::proxy::pipeline::middleware::ResponseMiddleware;
use crate::proxy::pipeline::values::{Context, Response};

pub struct HealthTrackMiddleware;

#[async_trait]
impl ResponseMiddleware for HealthTrackMiddleware {
    fn name(&self) -> &'static str {
        "health_track"
    }

    async fn on_response(&self, resp: Response, cx: &mut Context) -> Response {
        let status = match &resp {
            Response::McpBuffered { status, .. } | Response::McpStreamed { status, .. } => {
                Some(status.as_u16())
            }
            Response::Upstream502 { .. } => None,
            _ => return resp,
        };
        let is_init = matches!(
            cx.working.request_method,
            Some(ClientMethod::Lifecycle(LifecycleMethod::Initialize))
        );
        if let Some(code) = status
            && code < 400
            && is_init
        {
            lock_health(&cx.intake.proxy.health).confirm_mcp_connected();
        }
        // TODO: record per-request success / failure once the counter
        // API lands on ProxyHealth.
        resp
    }
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;

    use axum::body::Body;
    use axum::http::{HeaderMap, StatusCode};

    use crate::proxy::lock_health;
    use crate::proxy::pipeline::message::{ClientMethod, ToolsMethod};
    use crate::proxy::pipeline::middlewares::test_support::{
        mcp_buffered_response, set_request_method, test_context, test_proxy_state,
    };
    use crate::proxy::pipeline::values::Envelope;

    fn mcp_connected(proxy: &crate::proxy::ProxyState) -> bool {
        matches!(
            lock_health(&proxy.health).mcp_status,
            crate::proxy::ConnectionStatus::Connected
        )
    }

    #[tokio::test]
    async fn on_response__init_200_confirms_connected() {
        let proxy = test_proxy_state();
        let mut cx = test_context(proxy.clone());
        set_request_method(
            &mut cx,
            ClientMethod::Lifecycle(LifecycleMethod::Initialize),
        );
        let resp = mcp_buffered_response(r#"{"jsonrpc":"2.0","id":1,"result":{}}"#, StatusCode::OK);

        HealthTrackMiddleware.on_response(resp, &mut cx).await;
        assert!(mcp_connected(&proxy));
    }

    #[tokio::test]
    async fn on_response__non_init_200_does_not_confirm() {
        let proxy = test_proxy_state();
        let mut cx = test_context(proxy.clone());
        set_request_method(&mut cx, ClientMethod::Tools(ToolsMethod::List));
        let resp = mcp_buffered_response(
            r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[]}}"#,
            StatusCode::OK,
        );

        HealthTrackMiddleware.on_response(resp, &mut cx).await;
        assert!(!mcp_connected(&proxy));
    }

    #[tokio::test]
    async fn on_response__init_4xx_does_not_confirm() {
        let proxy = test_proxy_state();
        let mut cx = test_context(proxy.clone());
        set_request_method(
            &mut cx,
            ClientMethod::Lifecycle(LifecycleMethod::Initialize),
        );
        let resp = mcp_buffered_response(
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"bad"}}"#,
            StatusCode::BAD_REQUEST,
        );

        HealthTrackMiddleware.on_response(resp, &mut cx).await;
        assert!(!mcp_connected(&proxy));
    }

    #[tokio::test]
    async fn on_response__streamed_200_does_not_confirm_non_init() {
        let proxy = test_proxy_state();
        let mut cx = test_context(proxy.clone());
        set_request_method(&mut cx, ClientMethod::Tools(ToolsMethod::Call));
        let resp = Response::McpStreamed {
            envelope: Envelope::Json,
            body: Body::empty(),
            status: StatusCode::OK,
            headers: HeaderMap::new(),
        };

        HealthTrackMiddleware.on_response(resp, &mut cx).await;
        assert!(!mcp_connected(&proxy));
    }

    #[tokio::test]
    async fn on_response__upstream502_passthrough() {
        let proxy = test_proxy_state();
        let mut cx = test_context(proxy.clone());
        set_request_method(
            &mut cx,
            ClientMethod::Lifecycle(LifecycleMethod::Initialize),
        );
        let resp = Response::Upstream502 {
            reason: "down".into(),
        };

        let out = HealthTrackMiddleware.on_response(resp, &mut cx).await;
        assert!(matches!(out, Response::Upstream502 { .. }));
        assert!(!mcp_connected(&proxy));
    }

    #[tokio::test]
    async fn on_response__raw_passthrough() {
        let proxy = test_proxy_state();
        let mut cx = test_context(proxy.clone());
        let resp = Response::Raw {
            body: Body::empty(),
            status: StatusCode::OK,
            headers: HeaderMap::new(),
        };
        let out = HealthTrackMiddleware.on_response(resp, &mut cx).await;
        assert!(matches!(out, Response::Raw { .. }));
    }
}
