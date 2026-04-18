//! Stage ② — classify an already-parsed [`RequestContext`] into a
//! [`RouteKind`]. Reads pre-parsed `ctx.jsonrpc` / `ctx.wants_sse` directly;
//! the body is never re-parsed here.

use axum::http::{HeaderMap, Method, header};

use super::context::RequestContext;

/// Which handler serves this request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouteKind {
    /// Widget HTML page: `/widgets/{name}.html`
    WidgetHtml(String),
    /// Widget list: `/widgets` or `/widgets/`
    WidgetList,
    /// Static widget asset (JS, CSS, images, fonts)
    WidgetAsset,
    /// MCP JSON-RPC POST — body already parsed into `ctx.jsonrpc`.
    McpPost,
    /// MCP SSE GET (`Accept: text/event-stream`).
    McpSse,
    /// Everything else — forward to upstream untouched.
    Passthrough,
}

/// Classify the request into a [`RouteKind`].
pub fn route(ctx: &RequestContext, headers: &HeaderMap, has_widgets: bool) -> RouteKind {
    let path = ctx.path.as_str();

    if ctx.http_method == Method::GET {
        if let Some(name) = path
            .strip_prefix("/widgets/")
            .and_then(|s| s.strip_suffix(".html"))
        {
            return RouteKind::WidgetHtml(name.to_string());
        }
        if path == "/widgets" || path == "/widgets/" {
            return RouteKind::WidgetList;
        }
    }

    if ctx.http_method == Method::GET && has_widgets && is_widget_asset(path, headers) {
        return RouteKind::WidgetAsset;
    }

    if ctx.http_method == Method::POST && ctx.jsonrpc.is_some() {
        return RouteKind::McpPost;
    }

    if ctx.http_method == Method::GET && ctx.wants_sse {
        return RouteKind::McpSse;
    }

    RouteKind::Passthrough
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

    // ── McpPost ──

    #[test]
    fn mcp_post__jsonrpc_request() {
        let body = br#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
        let ctx = mk_ctx(Method::POST, "/mcp", &HeaderMap::new(), body);
        assert_eq!(route(&ctx, &HeaderMap::new(), false), RouteKind::McpPost);
    }

    #[test]
    fn mcp_post__rejects_oauth_register() {
        let body = br#"{"client_name":"My App","redirect_uris":["https://example.com/cb"]}"#;
        let ctx = mk_ctx(Method::POST, "/register", &HeaderMap::new(), body);
        assert_eq!(
            route(&ctx, &HeaderMap::new(), false),
            RouteKind::Passthrough
        );
    }

    #[test]
    fn mcp_post__rejects_form_encoded() {
        let body = b"grant_type=client_credentials&client_id=abc";
        let ctx = mk_ctx(Method::POST, "/token", &HeaderMap::new(), body);
        assert_eq!(
            route(&ctx, &HeaderMap::new(), false),
            RouteKind::Passthrough
        );
    }

    // ── McpSse ──

    #[test]
    fn mcp_sse__accepts_event_stream() {
        let mut headers = HeaderMap::new();
        headers.insert(header::ACCEPT, "text/event-stream".parse().unwrap());
        let ctx = mk_ctx(Method::GET, "/mcp", &headers, b"");
        assert_eq!(route(&ctx, &headers, false), RouteKind::McpSse);
    }

    #[test]
    fn mcp_sse__rejects_html() {
        let mut headers = HeaderMap::new();
        headers.insert(header::ACCEPT, "text/html".parse().unwrap());
        let ctx = mk_ctx(Method::GET, "/mcp", &headers, b"");
        // has_widgets=false → passthrough; has_widgets=true → widget asset
        assert_eq!(route(&ctx, &headers, false), RouteKind::Passthrough);
    }

    #[test]
    fn mcp_sse__rejects_no_accept() {
        let ctx = mk_ctx(Method::GET, "/mcp", &HeaderMap::new(), b"");
        assert_eq!(
            route(&ctx, &HeaderMap::new(), false),
            RouteKind::Passthrough
        );
    }

    // ── Widget routes ──

    #[test]
    fn widget_html__matches_prefix() {
        let ctx = mk_ctx(Method::GET, "/widgets/foo.html", &HeaderMap::new(), b"");
        assert_eq!(
            route(&ctx, &HeaderMap::new(), false),
            RouteKind::WidgetHtml("foo".to_string())
        );
    }

    #[test]
    fn widget_list__at_widgets_root() {
        let ctx = mk_ctx(Method::GET, "/widgets", &HeaderMap::new(), b"");
        assert_eq!(route(&ctx, &HeaderMap::new(), false), RouteKind::WidgetList);
    }

    #[test]
    fn widget_asset__js_extension() {
        let ctx = mk_ctx(Method::GET, "/assets/main.js", &HeaderMap::new(), b"");
        assert_eq!(route(&ctx, &HeaderMap::new(), true), RouteKind::WidgetAsset);
    }

    #[test]
    fn widget_asset__accept_image_with_widgets() {
        let mut headers = HeaderMap::new();
        headers.insert(header::ACCEPT, "image/png".parse().unwrap());
        let ctx = mk_ctx(Method::GET, "/logo", &headers, b"");
        assert_eq!(route(&ctx, &headers, true), RouteKind::WidgetAsset);
    }

    #[test]
    fn widget_asset__gated_by_has_widgets() {
        let ctx = mk_ctx(Method::GET, "/assets/main.js", &HeaderMap::new(), b"");
        // has_widgets=false — not a widget asset route
        assert_eq!(
            route(&ctx, &HeaderMap::new(), false),
            RouteKind::Passthrough
        );
    }

    #[test]
    fn widget_asset__rejects_well_known() {
        let ctx = mk_ctx(
            Method::GET,
            "/.well-known/oauth-authorization-server",
            &HeaderMap::new(),
            b"",
        );
        assert_eq!(route(&ctx, &HeaderMap::new(), true), RouteKind::Passthrough);
    }

    #[test]
    fn widget_asset__rejects_mcp_sse_accept() {
        let mut headers = HeaderMap::new();
        headers.insert(header::ACCEPT, "text/event-stream".parse().unwrap());
        let ctx = mk_ctx(Method::GET, "/mcp", &headers, b"");
        // SSE accept wins even with has_widgets=true
        assert_eq!(route(&ctx, &headers, true), RouteKind::McpSse);
    }
}
