//! Health-tracking step — flips the shared proxy health flag to
//! "connected" on a successful `initialize` response.
//!
//! Replaces `middleware::McpHealthMiddleware`. Doesn't need the body,
//! only status + method.

use crate::protocol::McpMethod;
use crate::proxy::ProxyState;

pub fn track_post_response(state: &ProxyState, method: &McpMethod, status: u16) {
    if *method == McpMethod::Initialize && status < 400 {
        crate::proxy::lock_health(&state.health).confirm_mcp_connected();
    }
}
