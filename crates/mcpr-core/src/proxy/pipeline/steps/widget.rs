//! Widget overlay step — for `resources/read` on `ui://widget/*` URIs,
//! swaps the upstream-returned HTML text for a locally-bundled copy.
//!
//! Replaces `middleware::WidgetOverlayMiddleware` as a plain function.

use crate::protocol::McpMethod;
use serde_json::Value;

use crate::proxy::ProxyState;
use crate::proxy::pipeline::context::RequestContext;
use crate::proxy::widgets::fetch_widget_html;

/// Attempt to overlay widget HTML into the parsed response JSON.
/// Returns `true` iff the JSON was mutated (caller should reserialize).
///
/// No-op unless:
/// - a widget source is configured on the proxy
/// - request method is `resources/read`
/// - request is not a batch
/// - request URI starts with `ui://widget/`
/// - a local widget HTML file is found matching that URI
pub async fn maybe_overlay(state: &ProxyState, req: &RequestContext, parsed: &mut Value) -> bool {
    if state.widget_source.is_none()
        || req.is_batch
        || req.mcp_method != Some(McpMethod::ResourcesRead)
    {
        return false;
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
        return false;
    };

    let Some(html) = fetch_widget_html(state, &uri).await else {
        return false;
    };

    let Some(contents) = parsed
        .get_mut("result")
        .and_then(|r| r.get_mut("contents"))
        .and_then(|c| c.as_array_mut())
    else {
        return false;
    };

    let mut mutated = false;
    for content in contents.iter_mut() {
        if content.get("text").is_some() {
            content["text"] = Value::String(html.clone());
            mutated = true;
        }
    }
    mutated
}
