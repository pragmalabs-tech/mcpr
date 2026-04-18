//! The sole construction site for [`RequestEvent`]. Every request path —
//! success, 502, short-circuit — calls [`emit_request_event`] exactly once
//! before returning. Handlers populate mutable ctx fields (session id,
//! client name/version) before calling in.

use crate::event::{ProxyEvent, RequestEvent};

use super::context::RequestContext;
use crate::proxy::proxy_state::ProxyState;

/// Response-side data the handler collects and passes to [`emit_request_event`].
pub struct ResponseSummary {
    pub status: u16,
    pub response_size: Option<u64>,
    pub upstream_us: Option<u64>,
    /// Stringified JSON-RPC error code (e.g. "-32600"). `None` for transport
    /// errors that don't carry a JSON-RPC code.
    pub error_code: Option<String>,
    /// Free-form error message. Truncated to 512 chars at emit time.
    pub error_msg: Option<String>,
}

impl ResponseSummary {
    /// Convenience for JSON-RPC errors — fills both code and message.
    pub fn with_rpc_error(mut self, code: i64, msg: impl Into<String>) -> Self {
        self.error_code = Some(code.to_string());
        self.error_msg = Some(msg.into());
        self
    }
}

/// Normalize a client name to a platform identifier used in `SessionStart`.
pub fn normalize_platform(client_name: &str) -> &'static str {
    let lower = client_name.to_lowercase();
    if lower.contains("claude") {
        "claude"
    } else if lower.contains("cursor") {
        "cursor"
    } else if lower.contains("chatgpt") || lower.contains("openai") {
        "chatgpt"
    } else if lower.contains("copilot") || lower.contains("vscode") || lower.contains("vs-code") {
        "vscode"
    } else if lower.contains("windsurf") {
        "windsurf"
    } else {
        "unknown"
    }
}

/// Build a `RequestEvent` from the accumulated request + response data.
/// Separated from [`emit_request_event`] so it can be unit-tested without a
/// live event bus.
pub fn build_request_event(
    proxy_name: &str,
    ctx: &RequestContext,
    resp: &ResponseSummary,
) -> RequestEvent {
    RequestEvent {
        id: uuid::Uuid::new_v4().to_string(),
        ts: chrono::Utc::now().timestamp_millis(),
        proxy: proxy_name.to_string(),
        session_id: ctx.session_id.clone(),
        method: ctx.http_method.to_string(),
        path: ctx.path.clone(),
        mcp_method: ctx.mcp_method_str.clone(),
        tool: ctx.tool.clone(),
        status: resp.status,
        latency_us: ctx.start.elapsed().as_micros() as u64,
        upstream_us: resp.upstream_us,
        request_size: Some(ctx.request_size as u64),
        response_size: resp.response_size,
        error_code: resp.error_code.clone(),
        error_msg: resp
            .error_msg
            .as_deref()
            .map(|m| m.chars().take(512).collect()),
        client_name: ctx.client_name.clone(),
        client_version: ctx.client_version.clone(),
        note: ctx.tags.join("+"),
    }
}

/// Emit a `Request` event to the proxy's event bus. The single construction
/// site for every request path. Handlers / middleware push onto `ctx.tags`
/// beforehand to set the event's `note` field.
pub fn emit_request_event(state: &ProxyState, ctx: &RequestContext, resp: &ResponseSummary) {
    state
        .event_bus
        .emit(ProxyEvent::Request(Box::new(build_request_event(
            &state.name,
            ctx,
            resp,
        ))));
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use std::time::Instant;

    use crate::protocol::McpMethod;
    use axum::http::Method;

    use super::*;
    use crate::proxy::pipeline::context::RequestContext;

    fn base_ctx() -> RequestContext {
        RequestContext {
            start: Instant::now(),
            http_method: Method::POST,
            path: "/mcp".to_string(),
            request_size: 128,
            wants_sse: false,
            session_id: None,
            jsonrpc: None,
            mcp_method: Some(McpMethod::ToolsCall),
            mcp_method_str: Some("tools/call".to_string()),
            tool: Some("search".to_string()),
            is_batch: false,
            client_info_from_init: None,
            client_name: None,
            client_version: None,
            tags: vec!["rewritten"],
        }
    }

    fn base_resp() -> ResponseSummary {
        ResponseSummary {
            status: 200,
            response_size: Some(256),
            upstream_us: Some(1000),
            error_code: None,
            error_msg: None,
        }
    }

    #[test]
    fn includes_client_info() {
        let mut ctx = base_ctx();
        ctx.session_id = Some("sess-1".into());
        ctx.client_name = Some("claude-desktop".into());
        ctx.client_version = Some("1.2.0".into());

        let ev = build_request_event("proxy", &ctx, &base_resp());
        assert_eq!(ev.client_name.as_deref(), Some("claude-desktop"));
        assert_eq!(ev.client_version.as_deref(), Some("1.2.0"));
        assert_eq!(ev.session_id.as_deref(), Some("sess-1"));
        assert_eq!(ev.tool.as_deref(), Some("search"));
        assert_eq!(ev.note, "rewritten");
        assert_eq!(ev.proxy, "proxy");
    }

    #[test]
    fn tags_join_with_plus() {
        let mut ctx = base_ctx();
        ctx.tags = vec!["rewritten", "sse"];
        let ev = build_request_event("proxy", &ctx, &base_resp());
        assert_eq!(ev.note, "rewritten+sse");
    }

    #[test]
    fn empty_tags__empty_note() {
        let mut ctx = base_ctx();
        ctx.tags.clear();
        let ev = build_request_event("proxy", &ctx, &base_resp());
        assert_eq!(ev.note, "");
    }

    #[test]
    fn none_client_info() {
        let mut ctx = base_ctx();
        ctx.tags = vec!["passthrough"];
        let mut resp = base_resp();
        resp.response_size = None;
        resp.upstream_us = None;
        let ev = build_request_event("proxy", &ctx, &resp);
        assert!(ev.client_name.is_none());
        assert!(ev.client_version.is_none());
        assert!(ev.session_id.is_none());
        assert!(ev.response_size.is_none());
        assert!(ev.upstream_us.is_none());
    }

    #[test]
    fn non_tool_call_tool_is_passthrough_from_ctx() {
        // Stage 1 parser leaves ctx.tool = None for non-tools/call methods.
        let mut ctx = base_ctx();
        ctx.mcp_method = Some(McpMethod::ResourcesRead);
        ctx.mcp_method_str = Some("resources/read".into());
        ctx.tool = None;
        ctx.client_name = Some("cursor".into());

        let ev = build_request_event("proxy", &ctx, &base_resp());
        assert!(ev.tool.is_none());
        assert_eq!(ev.client_name.as_deref(), Some("cursor"));
    }

    #[test]
    fn with_error() {
        let mut ctx = base_ctx();
        ctx.session_id = Some("sess-3".into());
        ctx.client_name = Some("vscode-copilot".into());
        ctx.client_version = Some("0.9.0".into());
        ctx.tags = vec!["upstream error"];
        let resp = ResponseSummary {
            status: 500,
            response_size: Some(80),
            upstream_us: Some(100),
            error_code: None,
            error_msg: None,
        }
        .with_rpc_error(-32600i64, "bad request");
        let ev = build_request_event("proxy", &ctx, &resp);
        assert_eq!(ev.error_code.as_deref(), Some("-32600"));
        assert_eq!(ev.error_msg.as_deref(), Some("bad request"));
        assert_eq!(ev.status, 500);
        assert_eq!(ev.note, "upstream error");
    }

    #[test]
    fn serializes_client_fields() {
        let mut ctx = base_ctx();
        ctx.session_id = Some("sess-1".into());
        ctx.client_name = Some("claude-desktop".into());
        ctx.client_version = Some("1.2.0".into());
        let ev = build_request_event("proxy", &ctx, &base_resp());
        let event = ProxyEvent::Request(Box::new(ev));
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"client_name\":\"claude-desktop\""));
        assert!(json.contains("\"client_version\":\"1.2.0\""));
        assert!(json.contains("\"type\":\"request\""));
    }

    #[test]
    fn omits_null_client_in_json() {
        let mut ctx = base_ctx();
        ctx.tool = None;
        let ev = build_request_event("proxy", &ctx, &base_resp());
        let event = ProxyEvent::Request(Box::new(ev));
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"client_name\":null"));
        assert!(json.contains("\"client_version\":null"));
    }

    #[test]
    fn normalize_platform__variants() {
        assert_eq!(normalize_platform("Claude Desktop"), "claude");
        assert_eq!(normalize_platform("cursor-editor"), "cursor");
        assert_eq!(normalize_platform("ChatGPT Plugin"), "chatgpt");
        assert_eq!(normalize_platform("OpenAI Agent"), "chatgpt");
        assert_eq!(normalize_platform("GitHub Copilot"), "vscode");
        assert_eq!(normalize_platform("VS-Code Extension"), "vscode");
        assert_eq!(normalize_platform("Windsurf IDE"), "windsurf");
        assert_eq!(normalize_platform("my-custom-client"), "unknown");
    }
}
