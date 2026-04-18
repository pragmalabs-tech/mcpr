//! Build a [`RequestContext`] from the raw HTTP request in a single pass.

use std::time::Instant;

use crate::protocol::session;
use crate::protocol::{self as jsonrpc, McpMethod};
use axum::body::Bytes;
use axum::http::{HeaderMap, Method, header};

use super::context::RequestContext;

/// Parse every field we'll need later from `(method, path, headers, body)`.
/// Never re-parsed downstream.
pub fn build_request_context(
    method: Method,
    path: &str,
    headers: &HeaderMap,
    body: &Bytes,
    start: Instant,
) -> RequestContext {
    let wants_sse = headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .map(|a| a.contains("text/event-stream"))
        .unwrap_or(false);

    let session_id = headers
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .map(String::from);

    let jsonrpc = jsonrpc::parse_body(body);
    let mcp_method = jsonrpc.as_ref().map(|p| p.mcp_method());
    let mcp_method_str = mcp_method.as_ref().map(|m| m.as_str().to_string());

    let tool = jsonrpc.as_ref().and_then(|p| {
        if p.mcp_method() == McpMethod::ToolsCall {
            p.detail()
        } else {
            None
        }
    });
    let is_batch = jsonrpc.as_ref().is_some_and(|p| p.is_batch);

    let client_info_from_init = if mcp_method == Some(McpMethod::Initialize) {
        jsonrpc
            .as_ref()
            .and_then(|p| p.first_params())
            .and_then(session::parse_client_info)
    } else {
        None
    };

    RequestContext {
        start,
        http_method: method,
        path: path.to_string(),
        request_size: body.len(),
        wants_sse,
        session_id,
        jsonrpc,
        mcp_method,
        mcp_method_str,
        tool,
        is_batch,
        client_info_from_init,
        client_name: None,
        client_version: None,
        tags: Vec::new(),
    }
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;

    fn mk(method: Method, headers: HeaderMap, body: &[u8]) -> RequestContext {
        build_request_context(
            method,
            "/mcp",
            &headers,
            &Bytes::copy_from_slice(body),
            Instant::now(),
        )
    }

    #[test]
    fn initialize__extracts_client_info() {
        let body = br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"clientInfo":{"name":"claude-desktop","version":"1.2.0"}}}"#;
        let ctx = mk(Method::POST, HeaderMap::new(), body);
        let info = ctx.client_info_from_init.expect("client info");
        assert_eq!(info.name, "claude-desktop");
        assert_eq!(info.version.as_deref(), Some("1.2.0"));
        assert_eq!(ctx.mcp_method, Some(McpMethod::Initialize));
        assert_eq!(ctx.mcp_method_str.as_deref(), Some("initialize"));
        assert!(ctx.tool.is_none());
    }

    #[test]
    fn tools_call__sets_tool_name() {
        let body = br#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"search"}}"#;
        let ctx = mk(Method::POST, HeaderMap::new(), body);
        assert_eq!(ctx.mcp_method, Some(McpMethod::ToolsCall));
        assert_eq!(ctx.tool.as_deref(), Some("search"));
    }

    #[test]
    fn batch__marks_is_batch() {
        let body = br#"[{"jsonrpc":"2.0","id":1,"method":"tools/list"},{"jsonrpc":"2.0","id":2,"method":"resources/list"}]"#;
        let ctx = mk(Method::POST, HeaderMap::new(), body);
        assert!(ctx.is_batch);
        assert!(ctx.jsonrpc.is_some());
    }

    #[test]
    fn non_json__jsonrpc_is_none() {
        let ctx = mk(Method::POST, HeaderMap::new(), b"not json at all");
        assert!(ctx.jsonrpc.is_none());
        assert!(ctx.mcp_method.is_none());
        assert!(ctx.mcp_method_str.is_none());
    }

    #[test]
    fn header__populates_session_id() {
        let mut headers = HeaderMap::new();
        headers.insert("mcp-session-id", "sess-abc".parse().unwrap());
        let ctx = mk(Method::POST, headers, b"");
        assert_eq!(ctx.session_id.as_deref(), Some("sess-abc"));
    }

    #[test]
    fn accept_sse__sets_wants_sse() {
        let mut headers = HeaderMap::new();
        headers.insert(header::ACCEPT, "text/event-stream".parse().unwrap());
        let ctx = mk(Method::GET, headers, b"");
        assert!(ctx.wants_sse);
    }

    #[test]
    fn request_size__reflects_body_bytes() {
        let body = br#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;
        let ctx = mk(Method::POST, HeaderMap::new(), body);
        assert_eq!(ctx.request_size, body.len());
    }

    #[test]
    fn non_initialize__no_client_info() {
        let body = br#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
        let ctx = mk(Method::POST, HeaderMap::new(), body);
        assert!(ctx.client_info_from_init.is_none());
    }
}
