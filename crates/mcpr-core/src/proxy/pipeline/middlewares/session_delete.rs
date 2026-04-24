//! Request-side middleware: on `DELETE` + `mcp-session-id`, tear down
//! the local session and emit `SessionEnd`, then let the transport
//! forward the DELETE upstream. (No short-circuit — forwarding the
//! DELETE lets the upstream release its own session state.)

use async_trait::async_trait;
use axum::http::Method;

use crate::event::{ProxyEvent, SessionEndEvent};
use crate::protocol::session::SessionStore;
use crate::proxy::pipeline::middleware::{Flow, RequestMiddleware};
use crate::proxy::pipeline::values::{Context, Request};

pub struct SessionDeleteMiddleware;

#[async_trait]
impl RequestMiddleware for SessionDeleteMiddleware {
    fn name(&self) -> &'static str {
        "session_delete"
    }

    async fn on_request(&self, req: Request, cx: &mut Context) -> Flow {
        let Request::Mcp(ref mcp) = req else {
            return Flow::Continue(req);
        };
        if cx.intake.http_method != Method::DELETE {
            return Flow::Continue(req);
        }
        let Some(sid) = mcp.session_hint.as_ref() else {
            return Flow::Continue(req);
        };

        let state = cx.intake.proxy.clone();
        state.sessions.remove(sid.as_str()).await;
        state
            .event_bus
            .emit(ProxyEvent::SessionEnd(SessionEndEvent {
                session_id: sid.as_str().to_string(),
                ts: chrono::Utc::now().timestamp_millis(),
            }));

        Flow::Continue(req)
    }
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;

    use axum::body::Body;
    use axum::http::HeaderMap;
    use serde_json::Value;

    use crate::proxy::pipeline::middlewares::test_support::{
        mcp_delete_request, mcp_request, test_context_with_method, test_proxy_with_sink,
    };
    use crate::proxy::pipeline::values::RawRequest;

    #[tokio::test]
    async fn on_request__delete_with_session_emits_and_removes() {
        let (proxy, sink, handle) = test_proxy_with_sink();
        proxy.sessions.create("sess-1").await;
        let mut cx = test_context_with_method(proxy.clone(), Method::DELETE);
        let req = mcp_delete_request(Some("sess-1"));

        let flow = SessionDeleteMiddleware.on_request(req, &mut cx).await;
        assert!(matches!(flow, Flow::Continue(Request::Mcp(_))));
        assert!(proxy.sessions.get("sess-1").await.is_none());

        handle.shutdown().await;
        let saw_end = sink
            .snapshot()
            .iter()
            .any(|e| matches!(e, ProxyEvent::SessionEnd(s) if s.session_id == "sess-1"));
        assert!(saw_end);
    }

    #[tokio::test]
    async fn on_request__delete_without_session_hint_is_noop() {
        let (proxy, sink, handle) = test_proxy_with_sink();
        let mut cx = test_context_with_method(proxy.clone(), Method::DELETE);
        let req = mcp_delete_request(None);

        SessionDeleteMiddleware.on_request(req, &mut cx).await;
        handle.shutdown().await;
        assert!(sink.snapshot().is_empty());
    }

    #[tokio::test]
    async fn on_request__non_delete_method_is_noop() {
        let (proxy, sink, handle) = test_proxy_with_sink();
        proxy.sessions.create("sess-2").await;
        let mut cx = test_context_with_method(proxy.clone(), Method::POST);
        let req = mcp_request("tools/list", Value::Null, Some("sess-2"));

        SessionDeleteMiddleware.on_request(req, &mut cx).await;
        assert!(proxy.sessions.get("sess-2").await.is_some());
        handle.shutdown().await;
        assert!(sink.snapshot().is_empty());
    }

    #[tokio::test]
    async fn on_request__non_mcp_is_noop() {
        let (proxy, sink, handle) = test_proxy_with_sink();
        let mut cx = test_context_with_method(proxy, Method::DELETE);
        let req = Request::Raw(RawRequest {
            method: Method::DELETE,
            path: "/something".into(),
            body: Body::empty(),
            headers: HeaderMap::new(),
        });

        SessionDeleteMiddleware.on_request(req, &mut cx).await;
        handle.shutdown().await;
        assert!(sink.snapshot().is_empty());
    }

    #[tokio::test]
    async fn on_request__unknown_session_still_emits() {
        let (proxy, sink, handle) = test_proxy_with_sink();
        let mut cx = test_context_with_method(proxy, Method::DELETE);
        let req = mcp_delete_request(Some("never-existed"));

        SessionDeleteMiddleware.on_request(req, &mut cx).await;
        handle.shutdown().await;
        let saw_end = sink
            .snapshot()
            .iter()
            .any(|e| matches!(e, ProxyEvent::SessionEnd(s) if s.session_id == "never-existed"));
        assert!(saw_end);
    }
}
