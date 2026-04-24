//! Response-side middleware: on a successful `initialize` response,
//! create the session, stash client info, emit `SessionStart`.
//!
//! Ports `pipeline/steps/session.rs::maybe_record_start` and absorbs the
//! `populate_client_info` helper that the old pipeline called
//! separately. Reads `cx.working.request_method` to detect the
//! originating method and `cx.working.client` for `ClientInfo` that
//! `ClientInfoInjectMiddleware` stashed on the request side.

use async_trait::async_trait;

use crate::event::{ProxyEvent, SessionStartEvent};
use crate::protocol::session::{SessionState, SessionStore};
use crate::proxy::pipeline::message::{ClientMethod, LifecycleMethod};
use crate::proxy::pipeline::middleware::ResponseMiddleware;
use crate::proxy::pipeline::values::{Context, Response};

use super::shared;

pub struct SessionRecordMiddleware;

#[async_trait]
impl ResponseMiddleware for SessionRecordMiddleware {
    fn name(&self) -> &'static str {
        "session_record"
    }

    async fn on_response(&self, resp: Response, cx: &mut Context) -> Response {
        let (status, sid) = match &resp {
            Response::McpBuffered {
                status, headers, ..
            } => {
                let sid = headers
                    .get("mcp-session-id")
                    .and_then(|v| v.to_str().ok())
                    .map(|s| s.to_string());
                (*status, sid)
            }
            _ => return resp,
        };
        if status.as_u16() >= 400 {
            return resp;
        }
        if !matches!(
            cx.working.request_method,
            Some(ClientMethod::Lifecycle(LifecycleMethod::Initialize))
        ) {
            return resp;
        }
        let Some(sid) = sid else { return resp };

        let state = cx.intake.proxy.clone();
        state.sessions.create(&sid).await;
        state
            .sessions
            .update_state(&sid, SessionState::Initialized)
            .await;

        let (client_name, client_version, client_platform) = match cx.working.client.clone() {
            Some(info) => {
                let platform = shared::normalize_platform(&info.name).to_string();
                let name = info.name.clone();
                let version = info.version.clone();
                state.sessions.set_client_info(&sid, info).await;
                (Some(name), version, Some(platform))
            }
            None => (None, None, None),
        };

        state
            .event_bus
            .emit(ProxyEvent::SessionStart(SessionStartEvent {
                session_id: sid,
                proxy: state.name.clone(),
                ts: chrono::Utc::now().timestamp_millis(),
                client_name,
                client_version,
                client_platform,
            }));

        resp
    }
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;

    use axum::http::StatusCode;

    use crate::protocol::session::ClientInfo;
    use crate::proxy::pipeline::middlewares::test_support::{
        mcp_buffered_response, mcp_buffered_response_with_header, set_request_method, test_context,
        test_proxy_with_sink,
    };

    #[tokio::test]
    async fn on_response__happy_path_creates_session_and_emits_event() {
        let (proxy, sink, handle) = test_proxy_with_sink();
        let mut cx = test_context(proxy.clone());
        set_request_method(
            &mut cx,
            ClientMethod::Lifecycle(LifecycleMethod::Initialize),
        );
        cx.working.client = Some(ClientInfo {
            name: "Claude Code".into(),
            version: Some("1.0.0".into()),
        });
        let resp = mcp_buffered_response_with_header(
            r#"{"jsonrpc":"2.0","id":1,"result":{"serverInfo":{"name":"srv"}}}"#,
            StatusCode::OK,
            "mcp-session-id",
            "abc-123",
        );

        SessionRecordMiddleware.on_response(resp, &mut cx).await;
        let info = proxy.sessions.get("abc-123").await.expect("session");
        assert_eq!(info.state, SessionState::Initialized);
        assert_eq!(info.client_info.as_ref().unwrap().name, "Claude Code");

        handle.shutdown().await;
        let events = sink.snapshot();
        let start_event = events
            .iter()
            .find_map(|e| match e {
                ProxyEvent::SessionStart(s) => Some(s),
                _ => None,
            })
            .expect("SessionStart emitted");
        assert_eq!(start_event.session_id, "abc-123");
        assert_eq!(start_event.client_platform.as_deref(), Some("claude"));
    }

    #[tokio::test]
    async fn on_response__4xx_skips() {
        let (proxy, sink, handle) = test_proxy_with_sink();
        let mut cx = test_context(proxy.clone());
        set_request_method(
            &mut cx,
            ClientMethod::Lifecycle(LifecycleMethod::Initialize),
        );
        let resp = mcp_buffered_response_with_header(
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"nope"}}"#,
            StatusCode::BAD_REQUEST,
            "mcp-session-id",
            "abc-123",
        );

        SessionRecordMiddleware.on_response(resp, &mut cx).await;
        assert!(proxy.sessions.get("abc-123").await.is_none());
        handle.shutdown().await;
        assert!(sink.snapshot().is_empty());
    }

    #[tokio::test]
    async fn on_response__missing_session_header_skips() {
        let (proxy, sink, handle) = test_proxy_with_sink();
        let mut cx = test_context(proxy.clone());
        set_request_method(
            &mut cx,
            ClientMethod::Lifecycle(LifecycleMethod::Initialize),
        );
        let resp = mcp_buffered_response(r#"{"jsonrpc":"2.0","id":1,"result":{}}"#, StatusCode::OK);

        SessionRecordMiddleware.on_response(resp, &mut cx).await;
        handle.shutdown().await;
        assert!(sink.snapshot().is_empty());
    }

    #[tokio::test]
    async fn on_response__non_initialize_skips() {
        let (proxy, sink, handle) = test_proxy_with_sink();
        let mut cx = test_context(proxy.clone());
        // request_method left as None simulates non-initialize
        let resp = mcp_buffered_response_with_header(
            r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[]}}"#,
            StatusCode::OK,
            "mcp-session-id",
            "abc-123",
        );

        SessionRecordMiddleware.on_response(resp, &mut cx).await;
        assert!(proxy.sessions.get("abc-123").await.is_none());
        handle.shutdown().await;
        assert!(sink.snapshot().is_empty());
    }

    #[tokio::test]
    async fn on_response__no_client_info_still_emits_with_none_fields() {
        let (proxy, sink, handle) = test_proxy_with_sink();
        let mut cx = test_context(proxy.clone());
        set_request_method(
            &mut cx,
            ClientMethod::Lifecycle(LifecycleMethod::Initialize),
        );
        let resp = mcp_buffered_response_with_header(
            r#"{"jsonrpc":"2.0","id":1,"result":{}}"#,
            StatusCode::OK,
            "mcp-session-id",
            "xyz",
        );

        SessionRecordMiddleware.on_response(resp, &mut cx).await;
        assert!(proxy.sessions.get("xyz").await.is_some());
        handle.shutdown().await;
        let start = sink
            .snapshot()
            .into_iter()
            .find_map(|e| match e {
                ProxyEvent::SessionStart(s) => Some(s),
                _ => None,
            })
            .expect("SessionStart emitted");
        assert!(start.client_name.is_none());
        assert!(start.client_platform.is_none());
    }

    #[tokio::test]
    async fn on_response__non_buffered_passthrough() {
        let (proxy, sink, handle) = test_proxy_with_sink();
        let mut cx = test_context(proxy.clone());
        set_request_method(
            &mut cx,
            ClientMethod::Lifecycle(LifecycleMethod::Initialize),
        );
        let resp = Response::Upstream502 { reason: "x".into() };

        SessionRecordMiddleware.on_response(resp, &mut cx).await;
        handle.shutdown().await;
        assert!(sink.snapshot().is_empty());
    }
}
