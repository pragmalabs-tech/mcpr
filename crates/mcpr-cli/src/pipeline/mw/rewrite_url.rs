//! Widget CSP rewrite — thin wrapper around
//! [`mcpr_core::proxy::rewrite_response`]. Applies only to MCP JSON-RPC
//! responses (requires a method string and parsed JSON).

use async_trait::async_trait;
use mcpr_core::proxy::rewrite_response;

use super::ResponseMw;
use crate::pipeline::context::{RequestContext, ResponseContext};
use crate::state::ProxyState;

pub struct UrlRewriteMw;

#[async_trait]
impl ResponseMw for UrlRewriteMw {
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
