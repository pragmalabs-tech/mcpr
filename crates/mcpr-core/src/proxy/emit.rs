//! `RequestEvent` construction and emission.
//!
//! The sole construction site for [`RequestEvent`]. `handle_request`
//! calls [`emit`] once, immediately after the response chain has
//! finished, before `IntoResponse` converts the value to an axum
//! response.
//!
//! `mcpr-cloud/backend/` consumes `RequestEvent` and session-lifecycle
//! events — the inline tests below cover the field shapes both rely on.

use crate::event::{ProxyEvent, RequestEvent};

use super::pipeline::middlewares::shared;
use super::pipeline::values::{Context, Envelope, Response, StageTiming};

/// Build a `RequestEvent` from the accumulated context and the final
/// response. Separated from [`emit`] so it can be unit-tested without a
/// live event bus.
pub fn build_request_event(proxy_name: &str, cx: &Context, resp: &Response) -> RequestEvent {
    let status = derive_status(resp);
    let response_size = derive_response_size(cx, resp);
    let (error_code, error_msg) = derive_error(resp);
    let (mcp_method_str, tool) = derive_method_and_tool(cx);
    let session_id = derive_session_id(cx, resp);
    let (client_name, client_version) = derive_client(cx);
    let note = derive_note(cx, resp);

    RequestEvent {
        id: uuid::Uuid::new_v4().to_string(),
        ts: chrono::Utc::now().timestamp_millis(),
        proxy: proxy_name.to_string(),
        session_id,
        method: cx.intake.http_method.to_string(),
        path: cx.intake.path.clone(),
        mcp_method: mcp_method_str,
        tool,
        resource_uri: cx.working.request_resource_uri.clone(),
        prompt_name: cx.working.request_prompt_name.clone(),
        status,
        latency_us: cx.intake.start.elapsed().as_micros() as u64,
        upstream_us: cx.working.upstream_us,
        request_size: Some(cx.intake.request_size as u64),
        response_size,
        error_code,
        error_msg: error_msg.map(|m| m.chars().take(512).collect()),
        client_name,
        client_version,
        note,
        stage_timings: derive_stage_timings(&cx.working.timings),
    }
}

fn derive_stage_timings(timings: &[StageTiming]) -> Option<Vec<StageTiming>> {
    if timings.is_empty() {
        None
    } else {
        Some(timings.to_vec())
    }
}

/// Emit a `Request` event to the proxy's event bus. Called once per
/// request, after the response chain.
pub fn emit(cx: &Context, resp: &Response) {
    let state = &cx.intake.proxy;
    state
        .event_bus
        .emit(ProxyEvent::Request(Box::new(build_request_event(
            &state.name,
            cx,
            resp,
        ))));
}

fn derive_status(resp: &Response) -> u16 {
    match resp {
        Response::McpBuffered { status, .. }
        | Response::McpStreamed { status, .. }
        | Response::OauthJson { status, .. }
        | Response::Raw { status, .. } => status.as_u16(),
        Response::Upstream502 { .. } => 502,
    }
}

fn derive_response_size(cx: &Context, _resp: &Response) -> Option<u64> {
    // `EnvelopeSealMiddleware` stashes the serialized buffered-body size
    // onto `cx.working.response_size`. Streaming paths and 502s leave it
    // unset — matching the legacy `response_size: None` on those paths.
    cx.working.response_size
}

fn derive_error(resp: &Response) -> (Option<String>, Option<String>) {
    match resp {
        Response::Upstream502 { reason } => (None, Some(reason.clone())),
        Response::McpBuffered { message, .. } => {
            if let Some(err) = &message.envelope.error {
                (Some(err.code.to_string()), Some(err.message.clone()))
            } else {
                (None, None)
            }
        }
        _ => (None, None),
    }
}

fn derive_method_and_tool(cx: &Context) -> (Option<String>, Option<String>) {
    // Legacy emit.rs tagged SSE GETs with `mcp_method = Some("SSE")` —
    // preserve that for cloud/backend continuity. Every other MCP method
    // comes from `cx.working.request_method` (set by SessionTouch).
    let http_get_is_sse = cx.intake.http_method == axum::http::Method::GET;
    let method_str = cx
        .working
        .request_method
        .as_ref()
        .and_then(crate::protocol::mcp::ClientMethod::as_str)
        .map(str::to_owned);
    let mcp_method_str = match (method_str, http_get_is_sse) {
        (Some(m), _) => Some(m),
        (None, true) => Some("SSE".to_owned()),
        (None, false) => None,
    };
    (mcp_method_str, cx.working.request_tool.clone())
}

fn derive_session_id(cx: &Context, resp: &Response) -> Option<String> {
    // Prefer the session we touched/created on the request side. Fall
    // back to reading the header off the response for `initialize`,
    // where `SessionRecord` creates the session but does not mutate
    // `cx.working.session`.
    if let Some(s) = cx.working.session.as_ref() {
        return Some(s.id.clone());
    }
    let headers = match resp {
        Response::McpBuffered { headers, .. }
        | Response::McpStreamed { headers, .. }
        | Response::Raw { headers, .. } => Some(headers),
        _ => None,
    };
    headers
        .and_then(|h| h.get("mcp-session-id"))
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}

fn derive_client(cx: &Context) -> (Option<String>, Option<String>) {
    // Request-side `ClientInfoInjectMiddleware` stashes `clientInfo` on
    // the initialize path. Subsequent requests read it from the session
    // record that `SessionTouchMiddleware` loaded into `cx.working.session`.
    if let Some(c) = cx.working.client.as_ref() {
        return (Some(c.name.clone()), c.version.clone());
    }
    if let Some(s) = cx.working.session.as_ref()
        && let Some(c) = s.client_info.as_ref()
    {
        return (Some(c.name.clone()), c.version.clone());
    }
    (None, None)
}

fn derive_note(cx: &Context, resp: &Response) -> String {
    // Middlewares push tags as they run. The Emitter appends shape-derived
    // tags that middlewares can't know about from their vantage point —
    // specifically `upstream error`, which only makes sense once we see
    // the final `Response::Upstream502` variant.
    let mut tags: Vec<&str> = cx.working.tags.as_slice().to_vec();
    if matches!(resp, Response::Upstream502 { .. }) && !tags.contains(&"upstream error") {
        tags.push("upstream error");
    }
    // For SSE legacy GETs the transport returns `Response::McpStreamed`
    // with `Envelope::Sse`. EnvelopeSeal does not touch streamed
    // responses, so no middleware tags it — surface from shape here to
    // match legacy `note = "sse"`.
    if matches!(
        resp,
        Response::McpStreamed {
            envelope: Envelope::Sse,
            ..
        }
    ) && !tags.contains(&"sse")
    {
        tags.push("sse");
    }
    tags.join("+")
}

/// Normalize a client name to a platform identifier used in `SessionStart`.
/// Thin re-export of `pipeline::middlewares::shared::normalize_platform`
/// so external callers of this module don't have to reach into the
/// middlewares submodule.
pub fn normalize_platform(client_name: &str) -> &'static str {
    shared::normalize_platform(client_name)
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;

    use axum::body::Body;
    use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};

    use crate::protocol::jsonrpc::JsonRpcEnvelope;
    use crate::protocol::mcp::{
        ClientMethod, LifecycleMethod, McpMessage, MessageKind, ServerKind, ToolsMethod,
    };
    use crate::proxy::pipeline::middlewares::test_support::{test_context, test_proxy_state};
    use crate::proxy::pipeline::values::{Envelope, Response};

    fn buffered_ok(body: &str) -> Response {
        let envelope = JsonRpcEnvelope::parse(body.as_bytes()).unwrap();
        let message = McpMessage {
            envelope,
            kind: MessageKind::Server(ServerKind::Result),
        };
        Response::McpBuffered {
            envelope: Envelope::Json,
            message,
            status: StatusCode::OK,
            headers: HeaderMap::new(),
        }
    }

    fn buffered_with_session(body: &str, session_id: &str) -> Response {
        let envelope = JsonRpcEnvelope::parse(body.as_bytes()).unwrap();
        let message = McpMessage {
            envelope,
            kind: MessageKind::Server(ServerKind::Result),
        };
        let mut headers = HeaderMap::new();
        headers.insert("mcp-session-id", HeaderValue::from_str(session_id).unwrap());
        Response::McpBuffered {
            envelope: Envelope::Json,
            message,
            status: StatusCode::OK,
            headers,
        }
    }

    #[tokio::test]
    async fn build__tools_list_200_ok_sets_method_and_status() {
        let proxy = test_proxy_state();
        let mut cx = test_context(proxy);
        cx.working.request_method = Some(ClientMethod::Tools(ToolsMethod::List));
        cx.working.tags.push("rewritten");

        let resp = buffered_ok(r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[]}}"#);
        let ev = build_request_event("test-proxy", &cx, &resp);

        assert_eq!(ev.status, 200);
        assert_eq!(ev.mcp_method.as_deref(), Some("tools/list"));
        assert_eq!(ev.proxy, "test-proxy");
        assert_eq!(ev.method, "POST");
        assert_eq!(ev.path, "/mcp");
        assert_eq!(ev.note, "rewritten");
        assert!(ev.error_code.is_none());
    }

    #[tokio::test]
    async fn build__rpc_error_in_buffered_result_surfaces_code_and_message() {
        let proxy = test_proxy_state();
        let mut cx = test_context(proxy);
        cx.working.request_method = Some(ClientMethod::Tools(ToolsMethod::List));
        let resp =
            buffered_ok(r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32600,"message":"bad"}}"#);
        let ev = build_request_event("p", &cx, &resp);
        assert_eq!(ev.error_code.as_deref(), Some("-32600"));
        assert_eq!(ev.error_msg.as_deref(), Some("bad"));
    }

    #[tokio::test]
    async fn build__upstream_502_tags_note_as_upstream_error() {
        let proxy = test_proxy_state();
        let cx = test_context(proxy);
        let resp = Response::Upstream502 {
            reason: "connection refused".into(),
        };
        let ev = build_request_event("p", &cx, &resp);
        assert_eq!(ev.status, 502);
        assert_eq!(ev.note, "upstream error");
        assert_eq!(ev.error_msg.as_deref(), Some("connection refused"));
    }

    #[tokio::test]
    async fn build__sse_streamed_response_tags_note_as_sse() {
        let proxy = test_proxy_state();
        let cx = test_context(proxy);
        let resp = Response::McpStreamed {
            envelope: Envelope::Sse,
            body: Body::empty(),
            status: StatusCode::OK,
            headers: HeaderMap::new(),
        };
        let ev = build_request_event("p", &cx, &resp);
        assert_eq!(ev.note, "sse");
    }

    #[tokio::test]
    async fn build__sse_get_without_stashed_method_reports_mcp_method_as_SSE_literal() {
        let proxy = test_proxy_state();
        let mut cx = test_context(proxy);
        cx.intake.http_method = Method::GET;
        let resp = Response::McpStreamed {
            envelope: Envelope::Sse,
            body: Body::empty(),
            status: StatusCode::OK,
            headers: HeaderMap::new(),
        };
        let ev = build_request_event("p", &cx, &resp);
        assert_eq!(ev.mcp_method.as_deref(), Some("SSE"));
    }

    #[tokio::test]
    async fn build__client_info_preferred_over_session_when_set() {
        use crate::protocol::session::ClientInfo;
        let proxy = test_proxy_state();
        let mut cx = test_context(proxy);
        cx.working.request_method = Some(ClientMethod::Lifecycle(LifecycleMethod::Initialize));
        cx.working.client = Some(ClientInfo {
            name: "claude-desktop".into(),
            version: Some("1.2.0".into()),
        });
        let resp = buffered_ok(r#"{"jsonrpc":"2.0","id":1,"result":{}}"#);
        let ev = build_request_event("p", &cx, &resp);
        assert_eq!(ev.client_name.as_deref(), Some("claude-desktop"));
        assert_eq!(ev.client_version.as_deref(), Some("1.2.0"));
    }

    #[tokio::test]
    async fn build__session_id_falls_back_to_response_header_when_working_session_empty() {
        let proxy = test_proxy_state();
        let mut cx = test_context(proxy);
        cx.working.request_method = Some(ClientMethod::Lifecycle(LifecycleMethod::Initialize));
        let resp = buffered_with_session(r#"{"jsonrpc":"2.0","id":1,"result":{}}"#, "sess-abc");
        let ev = build_request_event("p", &cx, &resp);
        assert_eq!(ev.session_id.as_deref(), Some("sess-abc"));
    }

    #[tokio::test]
    async fn build__session_id_uses_working_session_when_present() {
        use crate::protocol::session::SessionInfo;
        let proxy = test_proxy_state();
        let mut cx = test_context(proxy);
        cx.working.session = Some(SessionInfo {
            id: "sess-working".into(),
            state: crate::protocol::session::SessionState::Active,
            client_info: None,
            request_count: 0,
            created_at: chrono::Utc::now(),
            last_active: chrono::Utc::now(),
        });
        let resp = buffered_with_session(r#"{"jsonrpc":"2.0","id":1,"result":{}}"#, "sess-header");
        let ev = build_request_event("p", &cx, &resp);
        assert_eq!(ev.session_id.as_deref(), Some("sess-working"));
    }

    #[tokio::test]
    async fn build__stage_timings_propagated() {
        let proxy = test_proxy_state();
        let mut cx = test_context(proxy);
        cx.working.timings.push(StageTiming {
            name: "intake_parse",
            elapsed_us: 42,
        });
        cx.working.timings.push(StageTiming {
            name: "csp_rewrite",
            elapsed_us: 100,
        });
        let resp = buffered_ok(r#"{"jsonrpc":"2.0","id":1,"result":{}}"#);
        let ev = build_request_event("p", &cx, &resp);
        let timings = ev.stage_timings.expect("stage_timings populated");
        assert_eq!(timings.len(), 2);
        assert_eq!(timings[0].name, "intake_parse");
        assert_eq!(timings[0].elapsed_us, 42);
        assert_eq!(timings[1].name, "csp_rewrite");
    }

    #[tokio::test]
    async fn build__tags_joined_with_plus() {
        let proxy = test_proxy_state();
        let mut cx = test_context(proxy);
        cx.working.tags.push("rewritten");
        cx.working.tags.push("sse");
        let resp = buffered_ok(r#"{"jsonrpc":"2.0","id":1,"result":{}}"#);
        let ev = build_request_event("p", &cx, &resp);
        assert_eq!(ev.note, "rewritten+sse");
    }
}
