//! Widget CSP rewrite — thin wrapper around
//! [`crate::proxy::rewrite_response`]. Applies only to MCP JSON-RPC
//! responses (requires a method string and parsed JSON).

use crate::proxy::rewrite_response;
use async_trait::async_trait;

use super::ResponseMiddleware;
use crate::proxy::pipeline::context::{RequestContext, ResponseContext};
use crate::proxy::proxy_state::ProxyState;

pub struct UrlRewriteMiddleware;

#[async_trait]
impl ResponseMiddleware for UrlRewriteMiddleware {
    async fn on_response(
        &self,
        state: &ProxyState,
        req: &RequestContext,
        resp: &mut ResponseContext,
    ) {
        let (Some(method_str), Some(json)) = (&req.mcp_method_str, resp.json.as_mut()) else {
            return;
        };
        let config = state.rewrite_config.read().await;
        rewrite_response(method_str, json, &config);
    }
}
