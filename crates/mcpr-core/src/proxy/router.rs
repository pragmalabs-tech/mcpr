use crate::protocol as jsonrpc;
use axum::{
    body::Bytes,
    http::{HeaderMap, Method, header},
};

/// Classified request type for type-separate dispatch.
pub enum ClassifiedRequest {
    /// OAuth callback relay page
    OAuthCallback,
    /// Widget HTML page: /widgets/{name}.html
    WidgetHtml { name: String },
    /// Widget list: /widgets or /widgets/
    WidgetList,
    /// Static widget asset (JS, CSS, images, fonts)
    WidgetAsset,
    /// MCP JSON-RPC POST with parsed body
    McpPost { parsed: jsonrpc::ParsedBody },
    /// MCP SSE GET (text/event-stream)
    McpSse,
    /// Everything else → forward to upstream
    Passthrough,
}

/// Classify an incoming request for type-separate dispatch.
pub fn classify(
    method: &Method,
    path: &str,
    headers: &HeaderMap,
    body: &Bytes,
    has_widgets: bool,
) -> ClassifiedRequest {
    if *method == Method::GET && path == "/oauth/callback" {
        return ClassifiedRequest::OAuthCallback;
    }

    if *method == Method::GET {
        if let Some(name) = path
            .strip_prefix("/widgets/")
            .and_then(|s| s.strip_suffix(".html"))
        {
            return ClassifiedRequest::WidgetHtml {
                name: name.to_string(),
            };
        }
        if path == "/widgets" || path == "/widgets/" {
            return ClassifiedRequest::WidgetList;
        }
    }

    if *method == Method::GET && has_widgets && is_widget_asset(path, headers) {
        return ClassifiedRequest::WidgetAsset;
    }

    if *method == Method::POST
        && let Some(parsed) = parse_mcp_body(body)
    {
        return ClassifiedRequest::McpPost { parsed };
    }

    if *method == Method::GET && is_mcp_sse(headers) {
        return ClassifiedRequest::McpSse;
    }

    ClassifiedRequest::Passthrough
}

/// Check if a POST body is a valid JSON-RPC 2.0 message (MCP request).
fn parse_mcp_body(body: &Bytes) -> Option<jsonrpc::ParsedBody> {
    jsonrpc::parse_body(body)
}

/// Check if a GET request is an MCP SSE call based on accept header.
fn is_mcp_sse(headers: &HeaderMap) -> bool {
    headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .map(|a| a.contains("text/event-stream"))
        .unwrap_or(false)
}

/// Check if a request is for a static widget asset.
/// Uses both file extension and Accept header to decide.
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
    use super::*;

    // ── parse_mcp_body (JSON-RPC 2.0 detection) ──

    #[test]
    fn parse_mcp_body__jsonrpc_request() {
        let body = br#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
        let parsed = parse_mcp_body(&Bytes::from_static(body));
        assert!(parsed.is_some());
        let p = parsed.unwrap();
        assert_eq!(p.method_str(), "tools/list");
        assert!(!p.is_batch);
    }

    #[test]
    fn parse_mcp_body__jsonrpc_notification() {
        let body = br#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
        let parsed = parse_mcp_body(&Bytes::from_static(body));
        assert!(parsed.is_some());
        assert!(parsed.unwrap().is_notification_only());
    }

    #[test]
    fn parse_mcp_body__jsonrpc_batch() {
        let body = br#"[{"jsonrpc":"2.0","id":1,"method":"tools/list"},{"jsonrpc":"2.0","id":2,"method":"resources/list"}]"#;
        let parsed = parse_mcp_body(&Bytes::from_static(body));
        assert!(parsed.is_some());
        assert!(parsed.unwrap().is_batch);
    }

    #[test]
    fn parse_mcp_body__rejects_oauth_register() {
        let body = br#"{"client_name":"My App","redirect_uris":["https://example.com/cb"]}"#;
        assert!(parse_mcp_body(&Bytes::from_static(body)).is_none());
    }

    #[test]
    fn parse_mcp_body__rejects_form_encoded() {
        let body = b"grant_type=client_credentials&client_id=abc";
        assert!(parse_mcp_body(&Bytes::from_static(body)).is_none());
    }

    #[test]
    fn parse_mcp_body__rejects_empty() {
        assert!(parse_mcp_body(&Bytes::new()).is_none());
    }

    #[test]
    fn parse_mcp_body__rejects_wrong_version() {
        let body = br#"{"jsonrpc":"1.0","id":1,"method":"test"}"#;
        assert!(parse_mcp_body(&Bytes::from_static(body)).is_none());
    }

    // ── is_mcp_sse ──

    #[test]
    fn is_mcp_sse__accepts_event_stream() {
        let mut headers = HeaderMap::new();
        headers.insert(header::ACCEPT, "text/event-stream".parse().unwrap());
        assert!(is_mcp_sse(&headers));
    }

    #[test]
    fn is_mcp_sse__rejects_html() {
        let mut headers = HeaderMap::new();
        headers.insert(header::ACCEPT, "text/html".parse().unwrap());
        assert!(!is_mcp_sse(&headers));
    }

    #[test]
    fn is_mcp_sse__rejects_no_accept() {
        let headers = HeaderMap::new();
        assert!(!is_mcp_sse(&headers));
    }

    // ── is_widget_asset ──

    #[test]
    fn is_widget_asset__js_extension() {
        let headers = HeaderMap::new();
        assert!(is_widget_asset("/assets/main.js", &headers));
    }

    #[test]
    fn is_widget_asset__css_extension() {
        let headers = HeaderMap::new();
        assert!(is_widget_asset("/styles/app.css", &headers));
    }

    #[test]
    fn is_widget_asset__woff2_extension() {
        let headers = HeaderMap::new();
        assert!(is_widget_asset("/fonts/inter.woff2", &headers));
    }

    #[test]
    fn is_widget_asset__svg_extension() {
        let headers = HeaderMap::new();
        assert!(is_widget_asset("/icons/logo.svg", &headers));
    }

    #[test]
    fn is_widget_asset__accept_html() {
        let mut headers = HeaderMap::new();
        headers.insert(header::ACCEPT, "text/html".parse().unwrap());
        assert!(is_widget_asset("/some-path", &headers));
    }

    #[test]
    fn is_widget_asset__accept_image() {
        let mut headers = HeaderMap::new();
        headers.insert(header::ACCEPT, "image/png".parse().unwrap());
        assert!(is_widget_asset("/logo", &headers));
    }

    #[test]
    fn is_widget_asset__accept_javascript() {
        let mut headers = HeaderMap::new();
        headers.insert(header::ACCEPT, "application/javascript".parse().unwrap());
        assert!(is_widget_asset("/bundle", &headers));
    }

    #[test]
    fn is_widget_asset__rejects_well_known() {
        let headers = HeaderMap::new();
        assert!(!is_widget_asset(
            "/.well-known/oauth-authorization-server",
            &headers
        ));
    }

    #[test]
    fn is_widget_asset__rejects_mcp() {
        let headers = HeaderMap::new();
        assert!(!is_widget_asset("/mcp", &headers));
    }

    #[test]
    fn is_widget_asset__rejects_token() {
        let headers = HeaderMap::new();
        assert!(!is_widget_asset("/token", &headers));
    }

    #[test]
    fn is_widget_asset__rejects_authorize() {
        let headers = HeaderMap::new();
        assert!(!is_widget_asset("/authorize", &headers));
    }

    #[test]
    fn is_widget_asset__rejects_register() {
        let headers = HeaderMap::new();
        assert!(!is_widget_asset("/register", &headers));
    }

    #[test]
    fn is_widget_asset__rejects_json_accept() {
        let mut headers = HeaderMap::new();
        headers.insert(header::ACCEPT, "application/json".parse().unwrap());
        assert!(!is_widget_asset("/some-path", &headers));
    }

    #[test]
    fn is_widget_asset__rejects_sse_accept() {
        let mut headers = HeaderMap::new();
        headers.insert(header::ACCEPT, "text/event-stream".parse().unwrap());
        assert!(!is_widget_asset("/mcp", &headers));
    }
}
