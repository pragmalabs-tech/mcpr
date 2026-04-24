//! Classify an already-parsed [`RequestContext`] into a [`RequestKind`].
//! Reads pre-parsed `ctx.jsonrpc` / `ctx.wants_sse` directly; the body
//! is never re-parsed here.
//!
//! [`RequestKind`] is shape-aware: it splits MCP POST into `Buffer` vs
//! `Stream` based on whether the response may need mutation. The MCP
//! method is embedded so handlers don't need to re-look-up.

use axum::http::{HeaderMap, Method};

use super::context::RequestContext;
use crate::protocol::McpMethod;

/// Shape-aware classification the pipeline dispatches on.
#[derive(Debug, Clone, PartialEq)]
pub enum RequestKind {
    /// MCP POST whose response may need mutation (`tools/*`, `resources/*`).
    /// Handler buffers the body so it can scan and maybe-rewrite.
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
pub fn classify_request(ctx: &RequestContext, _headers: &HeaderMap) -> RequestKind {
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

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use std::time::Instant;

    use axum::body::Bytes;
    use axum::http::header;

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
            classify_request(&ctx, &HeaderMap::new()),
            RequestKind::McpPostBuffer(McpMethod::ToolsCall)
        );
    }

    #[test]
    fn classify__initialize_buffers_for_schema_capture() {
        // Initialize carries serverInfo/capabilities we want to record
        // in SchemaManager, so it takes the buffered path.
        let body = br#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#;
        let ctx = mk_ctx(Method::POST, "/mcp", &HeaderMap::new(), body);
        assert_eq!(
            classify_request(&ctx, &HeaderMap::new()),
            RequestKind::McpPostBuffer(McpMethod::Initialize)
        );
    }

    #[test]
    fn classify__ping_streams() {
        let body = br#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;
        let ctx = mk_ctx(Method::POST, "/mcp", &HeaderMap::new(), body);
        assert_eq!(
            classify_request(&ctx, &HeaderMap::new()),
            RequestKind::McpPostStream(McpMethod::Ping)
        );
    }

    #[test]
    fn classify__all_buffered_methods() {
        for (method_str, expected) in [
            ("initialize", McpMethod::Initialize),
            ("tools/list", McpMethod::ToolsList),
            ("tools/call", McpMethod::ToolsCall),
            ("resources/list", McpMethod::ResourcesList),
            (
                "resources/templates/list",
                McpMethod::ResourcesTemplatesList,
            ),
            ("resources/read", McpMethod::ResourcesRead),
            ("prompts/list", McpMethod::PromptsList),
        ] {
            let body =
                format!(r#"{{"jsonrpc":"2.0","id":1,"method":"{method_str}"}}"#).into_bytes();
            let ctx = mk_ctx(Method::POST, "/mcp", &HeaderMap::new(), &body);
            assert_eq!(
                classify_request(&ctx, &HeaderMap::new()),
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
            classify_request(&ctx, &HeaderMap::new()),
            RequestKind::Passthrough
        );

        let ctx = mk_ctx(
            Method::POST,
            "/token",
            &HeaderMap::new(),
            b"grant_type=x&client_id=y",
        );
        assert_eq!(
            classify_request(&ctx, &HeaderMap::new()),
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
            classify_request(&ctx, &headers),
            RequestKind::McpSseStream
        );
    }

    #[test]
    fn classify__get_html_is_passthrough() {
        let mut headers = HeaderMap::new();
        headers.insert(header::ACCEPT, "text/html".parse().unwrap());
        let ctx = mk_ctx(Method::GET, "/mcp", &headers, b"");
        assert_eq!(
            classify_request(&ctx, &headers),
            RequestKind::Passthrough
        );
    }

    // ── Widget paths now passthrough (regression guards for Phase 1) ──
    //
    // Widget static-serving was removed. These three tests fail loudly if
    // anyone reintroduces a `/widgets/...` or static-asset branch.

    #[test]
    fn classify__widget_html_path_is_now_passthrough() {
        let ctx = mk_ctx(Method::GET, "/widgets/foo.html", &HeaderMap::new(), b"");
        assert_eq!(
            classify_request(&ctx, &HeaderMap::new()),
            RequestKind::Passthrough
        );
    }

    #[test]
    fn classify__widget_list_path_is_now_passthrough() {
        let ctx = mk_ctx(Method::GET, "/widgets", &HeaderMap::new(), b"");
        assert_eq!(
            classify_request(&ctx, &HeaderMap::new()),
            RequestKind::Passthrough
        );
    }

    #[test]
    fn classify__static_asset_extension_is_now_passthrough() {
        let ctx = mk_ctx(Method::GET, "/assets/main.js", &HeaderMap::new(), b"");
        assert_eq!(
            classify_request(&ctx, &HeaderMap::new()),
            RequestKind::Passthrough
        );
    }
}
