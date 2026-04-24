//! Request-side middleware: touch the session store, stash the
//! originating `ClientMethod` on `Working` for response middlewares.

use async_trait::async_trait;

use crate::protocol::session::{SessionState, SessionStore};
use crate::proxy::pipeline::message::{ClientKind, ClientNotifMethod};
use crate::proxy::pipeline::middleware::{Flow, RequestMiddleware};
use crate::proxy::pipeline::values::{Context, Request};

pub struct SessionTouchMiddleware;

#[async_trait]
impl RequestMiddleware for SessionTouchMiddleware {
    fn name(&self) -> &'static str {
        "session_touch"
    }

    async fn on_request(&self, req: Request, cx: &mut Context) -> Flow {
        let Request::Mcp(ref mcp) = req else {
            return Flow::Continue(req);
        };

        if let ClientKind::Request(m) = &mcp.kind {
            cx.working.request_method = Some(m.clone());
        }

        if let Some(sid) = mcp.session_hint.as_ref() {
            let store = &cx.intake.proxy.sessions;
            store.touch(sid.as_str()).await;
            if matches!(
                mcp.kind,
                ClientKind::Notification(ClientNotifMethod::Initialized)
            ) {
                store.update_state(sid.as_str(), SessionState::Active).await;
            }
            cx.working.session = store.get(sid.as_str()).await;
        }

        Flow::Continue(req)
    }
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;

    use axum::body::Body;
    use axum::http::{HeaderMap, Method};
    use serde_json::Value;

    use crate::proxy::pipeline::message::{ClientMethod, ToolsMethod};
    use crate::proxy::pipeline::middlewares::test_support::{
        mcp_notification, mcp_request, test_context, test_proxy_state,
    };
    use crate::proxy::pipeline::values::{RawRequest, Request};

    #[tokio::test]
    async fn on_request__non_mcp_passthrough() {
        let proxy = test_proxy_state();
        let mut cx = test_context(proxy);
        let req = Request::Raw(RawRequest {
            method: Method::GET,
            path: "/health".into(),
            body: Body::empty(),
            headers: HeaderMap::new(),
        });

        let flow = SessionTouchMiddleware.on_request(req, &mut cx).await;
        assert!(matches!(flow, Flow::Continue(Request::Raw(_))));
        assert!(cx.working.session.is_none());
        assert!(cx.working.request_method.is_none());
    }

    #[tokio::test]
    async fn on_request__mcp_no_session_hint_still_stashes_method() {
        let proxy = test_proxy_state();
        let mut cx = test_context(proxy);
        let req = mcp_request("tools/list", Value::Null, None);

        SessionTouchMiddleware.on_request(req, &mut cx).await;
        assert_eq!(
            cx.working.request_method,
            Some(ClientMethod::Tools(ToolsMethod::List))
        );
        assert!(cx.working.session.is_none());
    }

    #[tokio::test]
    async fn on_request__known_session_bumps_request_count() {
        let proxy = test_proxy_state();
        proxy.sessions.create("sess-1").await;
        let mut cx = test_context(proxy.clone());
        let req = mcp_request("tools/list", Value::Null, Some("sess-1"));

        SessionTouchMiddleware.on_request(req, &mut cx).await;
        let info = proxy.sessions.get("sess-1").await.unwrap();
        assert_eq!(info.request_count, 1);
        assert_eq!(cx.working.session.as_ref().unwrap().id, "sess-1");
    }

    #[tokio::test]
    async fn on_request__initialized_notification_flips_state_to_active() {
        let proxy = test_proxy_state();
        proxy.sessions.create("sess-2").await;
        proxy
            .sessions
            .update_state("sess-2", SessionState::Initialized)
            .await;
        let mut cx = test_context(proxy.clone());
        let req = mcp_notification("notifications/initialized", Some("sess-2"));

        SessionTouchMiddleware.on_request(req, &mut cx).await;
        let info = proxy.sessions.get("sess-2").await.unwrap();
        assert_eq!(info.state, SessionState::Active);
        assert!(cx.working.request_method.is_none());
    }

    #[tokio::test]
    async fn on_request__unknown_session_id_is_noop() {
        let proxy = test_proxy_state();
        let mut cx = test_context(proxy.clone());
        let req = mcp_request("tools/list", Value::Null, Some("missing"));

        SessionTouchMiddleware.on_request(req, &mut cx).await;
        assert!(cx.working.session.is_none());
        assert!(proxy.sessions.get("missing").await.is_none());
    }
}
