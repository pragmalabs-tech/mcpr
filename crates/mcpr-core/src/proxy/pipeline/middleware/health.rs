//! `McpHealthMiddleware` — on a successful `initialize` response, mark the MCP
//! upstream as confirmed connected.

use crate::protocol::McpMethod;
use async_trait::async_trait;

use super::ResponseMiddleware;
use crate::proxy::pipeline::context::{RequestContext, ResponseContext};
use crate::proxy::proxy_state::ProxyState;

pub struct McpHealthMiddleware;

#[async_trait]
impl ResponseMiddleware for McpHealthMiddleware {
    async fn on_response(
        &self,
        state: &ProxyState,
        req: &RequestContext,
        resp: &mut ResponseContext,
    ) {
        if req.mcp_method == Some(McpMethod::Initialize) && resp.status < 400 {
            crate::proxy::lock_health(&state.health).confirm_mcp_connected();
        }
    }
}
