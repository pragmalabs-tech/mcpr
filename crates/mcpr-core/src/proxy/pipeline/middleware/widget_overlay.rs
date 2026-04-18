//! `WidgetOverlayMiddleware` — substitute upstream-returned widget HTML with a local
//! bundle for `ui://widget/*` resources. Runs only when:
//!
//! * the proxy has a widget source configured,
//! * the request is a non-batch `resources/read`, and
//! * the requested URI matches `ui://widget/<name>(.html)?`.
//!
//! The upstream response is still issued (handles auth, meta, CSP). This middleware
//! only swaps the `text` field of each matching `contents[]` entry.

use crate::protocol::McpMethod;
use async_trait::async_trait;
use serde_json::Value;

use super::ResponseMiddleware;
use crate::proxy::pipeline::context::{RequestContext, ResponseContext};
use crate::proxy::proxy_state::ProxyState;
use crate::proxy::widgets::fetch_widget_html;

pub struct WidgetOverlayMiddleware;

#[async_trait]
impl ResponseMiddleware for WidgetOverlayMiddleware {
    async fn on_response(
        &self,
        state: &ProxyState,
        req: &RequestContext,
        resp: &mut ResponseContext,
    ) {
        if state.widget_source.is_none()
            || req.is_batch
            || req.mcp_method != Some(McpMethod::ResourcesRead)
        {
            return;
        }
        let Some(uri) = req
            .jsonrpc
            .as_ref()
            .and_then(|p| p.first_params())
            .and_then(|params| params.get("uri"))
            .and_then(|u| u.as_str())
            .and_then(|u| u.strip_prefix("ui://widget/"))
            .map(|s| s.trim_end_matches(".html").to_string())
        else {
            return;
        };

        let Some(html) = fetch_widget_html(state, &uri).await else {
            return;
        };

        let Some(json) = resp.json.as_mut() else {
            return;
        };
        if let Some(contents) = json
            .get_mut("result")
            .and_then(|r| r.get_mut("contents"))
            .and_then(|c| c.as_array_mut())
        {
            for content in contents.iter_mut() {
                if content.get("text").is_some() {
                    content["text"] = Value::String(html.clone());
                }
            }
        }
    }
}
