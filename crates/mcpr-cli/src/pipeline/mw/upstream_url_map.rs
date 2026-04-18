//! Passthrough URL substitution — swaps upstream base URL for the proxy URL
//! inside JSON response bodies so clients following server-returned links
//! stay on the proxy.
//!
//! Different from [`super::UrlRewriteMw`]: that one does structured CSP
//! rewriting on MCP responses. This one does naive string replacement on
//! non-MCP JSON (OAuth endpoints, health probes, etc.) — different concerns,
//! different algorithms.

use async_trait::async_trait;
use axum::http::header;
use mcpr_core::proxy::sse::split_upstream;

use super::ResponseMw;
use crate::pipeline::context::{RequestContext, ResponseContext};
use crate::state::ProxyState;

pub struct UpstreamUrlMapMw;

#[async_trait]
impl ResponseMw for UpstreamUrlMapMw {
    async fn on_response(
        &self,
        state: &ProxyState,
        _req: &RequestContext,
        resp: &mut ResponseContext,
    ) {
        let is_json = resp
            .headers
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|ct| ct.contains("json"))
            .unwrap_or(false);
        if !is_json {
            return;
        }
        let config = state.rewrite_config.read().await;
        let (upstream_base, _) = split_upstream(&config.mcp_upstream);
        let body_str = String::from_utf8_lossy(&resp.body);
        let rewritten = body_str
            .replace(
                upstream_base.trim_end_matches('/'),
                config.proxy_url.trim_end_matches('/'),
            )
            .into_bytes();
        resp.body = rewritten;
    }
}
