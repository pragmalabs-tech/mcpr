//! Classify an already-parsed [`RequestContext`] into a [`RequestKind`].
//! Reads pre-parsed `ctx.jsonrpc` / `ctx.wants_sse` directly; the body
//! is never re-parsed here.
//!
//! [`RequestKind`] is shape-aware: it splits MCP POST into `Buffer` vs
//! `Stream` based on whether the response may need mutation. The MCP
//! method is embedded so handlers don't need to re-look-up.

use axum::http::{HeaderMap, Method, header};

use super::context::RequestContext;
use crate::protocol::McpMethod;

/// Shape-aware classification the pipeline dispatches on.
#[derive(Debug, Clone, PartialEq)]
pub enum RequestKind {
    /// Widget HTML page: `GET /widgets/{name}.html`.
    WidgetHtml(String),
    /// Widget list page: `GET /widgets[/]`.
    WidgetList,
    /// Static widget asset (JS/CSS/images/fonts) served via widget source.
    WidgetAsset,
    /// MCP POST whose response may need mutation (`tools/*`, `resources/*`).
    /// Handler buffers the body so it can scan, maybe-rewrite, maybe-overlay.
    McpPostBuffer(McpMethod),
    /// MCP POST whose response never needs mutation (initialize, ping,
    /// notifications, prompts, completion, logging). Handler streams the
    /// body straight through.
    McpPostStream(McpMethod),
    /// GET /mcp with `Accept: text/event-stream` — long-lived stream.
    McpSseStream,
    /// Everything else — forward to upstream, stream bytes through.
    Passthrough,
}

/// Classify the request. Pure function: only reads its inputs.
pub fn classify_request(
    ctx: &RequestContext,
    headers: &HeaderMap,
    has_widgets: bool,
) -> RequestKind {
    let path = ctx.path.as_str();

    // Widget HTML / list live under /widgets regardless of widget source.
    if ctx.http_method == Method::GET {
        if let Some(name) = path
            .strip_prefix("/widgets/")
            .and_then(|s| s.strip_suffix(".html"))
        {
            return RequestKind::WidgetHtml(name.to_string());
        }
        if path == "/widgets" || path == "/widgets/" {
            return RequestKind::WidgetList;
        }
    }

    // Static widget assets only when a widget source is configured.
    if ctx.http_method == Method::GET && has_widgets && is_widget_asset(path, headers) {
        return RequestKind::WidgetAsset;
    }

    // MCP POST — the most common case. Split on whether the response
    // can be mutated for this method.
    if ctx.http_method == Method::POST && ctx.jsonrpc.is_some() {
        let method = ctx
            .mcp_method
            .clone()
            .unwrap_or_else(|| McpMethod::Unknown(String::new()));
        return if method.needs_response_buffering() {
            RequestKind::McpPostBuffer(method)
        } else {
            RequestKind::McpPostStream(method)
        };
    }

    // MCP SSE stream — GET /mcp with Accept: text/event-stream.
    if ctx.http_method == Method::GET && ctx.wants_sse {
        return RequestKind::McpSseStream;
    }

    RequestKind::Passthrough
}

/// `true` when the path or Accept header looks like a static asset a widget
/// bundle would request.
fn is_widget_asset(path: &str, headers: &HeaderMap) -> bool {
    let ext = path.rsplit('.').next().unwrap_or("");
    if matches!(
        ext,
        "js" | "mjs"
            | "css"
            | "html"
            | "svg"
            | "png"
            | "jpg"
            | "jpeg"
            | "gif"
            | "ico"
            | "woff"
            | "woff2"
            | "ttf"
            | "eot"
            | "map"
            | "webp"
    ) {
        return true;
    }

    if let Some(accept) = headers.get(header::ACCEPT).and_then(|v| v.to_str().ok())
        && (accept.contains("text/html")
            || accept.contains("text/css")
            || accept.contains("image/")
            || accept.contains("font/")
            || accept.contains("application/javascript"))
    {
        return true;
    }

    false
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use std::time::Instant;

    use axum::body::Bytes;

    use super::*;
    use crate::proxy::pipeline::parser::build_request_context;

    fn mk_ctx(method: Method, path: &str, headers: &HeaderMap, body: &[u8]) -> RequestContext {
        build_request_context(
            method,
            path,
            headers,
            &Bytes::copy_from_slice(body),
            Instant::now(),
        )
    }

    // ── MCP POST classifications ──

    #[test]
    fn classify__tools_call_needs_buffer() {
        let body = br#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"echo"}}"#;
        let ctx = mk_ctx(Method::POST, "/mcp", &HeaderMap::new(), body);
        assert_eq!(
            classify_request(&ctx, &HeaderMap::new(), false),
            RequestKind::McpPostBuffer(McpMethod::ToolsCall)
        );
    }

    #[test]
    fn classify__initialize_streams() {
        let body = br#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#;
        let ctx = mk_ctx(Method::POST, "/mcp", &HeaderMap::new(), body);
        assert_eq!(
            classify_request(&ctx, &HeaderMap::new(), false),
            RequestKind::McpPostStream(McpMethod::Initialize)
        );
    }

    #[test]
    fn classify__ping_streams() {
        let body = br#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;
        let ctx = mk_ctx(Method::POST, "/mcp", &HeaderMap::new(), body);
        assert_eq!(
            classify_request(&ctx, &HeaderMap::new(), false),
            RequestKind::McpPostStream(McpMethod::Ping)
        );
    }

    #[test]
    fn classify__all_buffered_methods() {
        for (method_str, expected) in [
            ("tools/list", McpMethod::ToolsList),
            ("tools/call", McpMethod::ToolsCall),
            ("resources/list", McpMethod::ResourcesList),
            (
                "resources/templates/list",
                McpMethod::ResourcesTemplatesList,
            ),
            ("resources/read", McpMethod::ResourcesRead),
        ] {
            let body =
                format!(r#"{{"jsonrpc":"2.0","id":1,"method":"{method_str}"}}"#).into_bytes();
            let ctx = mk_ctx(Method::POST, "/mcp", &HeaderMap::new(), &body);
            assert_eq!(
                classify_request(&ctx, &HeaderMap::new(), false),
                RequestKind::McpPostBuffer(expected),
                "method {method_str} should route to McpPostBuffer"
            );
        }
    }

    #[test]
    fn classify__non_mcp_post_is_passthrough() {
        let body = br#"{"client_name":"My App"}"#;
        let ctx = mk_ctx(Method::POST, "/register", &HeaderMap::new(), body);
        assert_eq!(
            classify_request(&ctx, &HeaderMap::new(), false),
            RequestKind::Passthrough
        );

        let ctx = mk_ctx(
            Method::POST,
            "/token",
            &HeaderMap::new(),
            b"grant_type=x&client_id=y",
        );
        assert_eq!(
            classify_request(&ctx, &HeaderMap::new(), false),
            RequestKind::Passthrough
        );
    }

    // ── SSE ──

    #[test]
    fn classify__sse_stream() {
        let mut headers = HeaderMap::new();
        headers.insert(header::ACCEPT, "text/event-stream".parse().unwrap());
        let ctx = mk_ctx(Method::GET, "/mcp", &headers, b"");
        assert_eq!(
            classify_request(&ctx, &headers, false),
            RequestKind::McpSseStream
        );
    }

    #[test]
    fn classify__get_html_is_passthrough() {
        let mut headers = HeaderMap::new();
        headers.insert(header::ACCEPT, "text/html".parse().unwrap());
        let ctx = mk_ctx(Method::GET, "/mcp", &headers, b"");
        // has_widgets=false — no widget asset route, HTML accept doesn't imply SSE.
        assert_eq!(
            classify_request(&ctx, &headers, false),
            RequestKind::Passthrough
        );
    }

    #[test]
    fn classify__sse_accept_wins_over_widgets() {
        let mut headers = HeaderMap::new();
        headers.insert(header::ACCEPT, "text/event-stream".parse().unwrap());
        let ctx = mk_ctx(Method::GET, "/mcp", &headers, b"");
        assert_eq!(
            classify_request(&ctx, &headers, true),
            RequestKind::McpSseStream
        );
    }

    // ── Widget routes ──

    #[test]
    fn classify__widget_html_matches_prefix() {
        let ctx = mk_ctx(Method::GET, "/widgets/foo.html", &HeaderMap::new(), b"");
        assert_eq!(
            classify_request(&ctx, &HeaderMap::new(), false),
            RequestKind::WidgetHtml("foo".to_string())
        );
    }

    #[test]
    fn classify__widget_list_at_widgets_root() {
        let ctx = mk_ctx(Method::GET, "/widgets", &HeaderMap::new(), b"");
        assert_eq!(
            classify_request(&ctx, &HeaderMap::new(), false),
            RequestKind::WidgetList
        );
    }

    #[test]
    fn classify__widget_asset_js_with_widgets() {
        let ctx = mk_ctx(Method::GET, "/assets/main.js", &HeaderMap::new(), b"");
        assert_eq!(
            classify_request(&ctx, &HeaderMap::new(), true),
            RequestKind::WidgetAsset
        );
    }

    #[test]
    fn classify__widget_asset_image_accept_with_widgets() {
        let mut headers = HeaderMap::new();
        headers.insert(header::ACCEPT, "image/png".parse().unwrap());
        let ctx = mk_ctx(Method::GET, "/logo", &headers, b"");
        assert_eq!(
            classify_request(&ctx, &headers, true),
            RequestKind::WidgetAsset
        );
    }

    #[test]
    fn classify__widget_asset_gated_by_has_widgets() {
        let ctx = mk_ctx(Method::GET, "/assets/main.js", &HeaderMap::new(), b"");
        assert_eq!(
            classify_request(&ctx, &HeaderMap::new(), false),
            RequestKind::Passthrough
        );
    }

    #[test]
    fn classify__well_known_not_widget_asset() {
        let ctx = mk_ctx(
            Method::GET,
            "/.well-known/oauth-authorization-server",
            &HeaderMap::new(),
            b"",
        );
        assert_eq!(
            classify_request(&ctx, &HeaderMap::new(), true),
            RequestKind::Passthrough
        );
    }
}
