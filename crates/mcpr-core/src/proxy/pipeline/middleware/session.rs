//! Session lifecycle middleware — covers both request and response phases.
//!
//! * `SessionTouchMiddleware` (request): bumps `last_seen` and flips Initialized→Active
//!   when the client sends `notifications/initialized`.
//! * `DeleteSessionEndMiddleware` (request): on `DELETE` with an `mcp-session-id`
//!   header, emits `SessionEnd` and removes the session.
//! * `SessionStartMiddleware` (response): on a successful `initialize` response,
//!   creates the session, stores parsed client info, and emits `SessionStart`.

use crate::event::{ProxyEvent, SessionEndEvent, SessionStartEvent};
use crate::protocol::McpMethod;
use crate::protocol::session::{SessionState, SessionStore};
use async_trait::async_trait;
use axum::http::Method;
use axum::response::Response;

use super::{RequestMiddleware, ResponseMiddleware};
use crate::proxy::pipeline::context::{RequestContext, ResponseContext};
use crate::proxy::pipeline::emit::normalize_platform;
use crate::proxy::proxy_state::ProxyState;

pub struct SessionTouchMiddleware;

#[async_trait]
impl RequestMiddleware for SessionTouchMiddleware {
    async fn on_request(&self, state: &ProxyState, ctx: &mut RequestContext) -> Option<Response> {
        let sid = ctx.session_id.as_deref()?;
        state.sessions.touch(sid).await;
        if ctx.mcp_method == Some(McpMethod::Initialized) {
            state.sessions.update_state(sid, SessionState::Active).await;
        }
        None
    }
}

pub struct DeleteSessionEndMiddleware;

#[async_trait]
impl RequestMiddleware for DeleteSessionEndMiddleware {
    async fn on_request(&self, state: &ProxyState, ctx: &mut RequestContext) -> Option<Response> {
        if ctx.http_method != Method::DELETE {
            return None;
        }
        let sid = ctx.session_id.as_deref()?;
        state
            .event_bus
            .emit(ProxyEvent::SessionEnd(SessionEndEvent {
                session_id: sid.to_string(),
                ts: chrono::Utc::now().timestamp_millis(),
            }));
        state.sessions.remove(sid).await;
        None
    }
}

pub struct SessionStartMiddleware;

#[async_trait]
impl ResponseMiddleware for SessionStartMiddleware {
    async fn on_response(
        &self,
        state: &ProxyState,
        req: &RequestContext,
        resp: &mut ResponseContext,
    ) {
        if req.mcp_method != Some(McpMethod::Initialize) || resp.status >= 400 {
            return;
        }
        let Some(sid) = req.session_id.as_deref() else {
            return;
        };

        state.sessions.create(sid).await;
        state
            .sessions
            .update_state(sid, SessionState::Initialized)
            .await;

        let (client_name, client_version, client_platform) =
            if let Some(info) = req.client_info_from_init.clone() {
                let platform = normalize_platform(&info.name).to_string();
                let name = info.name.clone();
                let version = info.version.clone();
                state.sessions.set_client_info(sid, info).await;
                (Some(name), version, Some(platform))
            } else {
                (None, None, None)
            };

        state
            .event_bus
            .emit(ProxyEvent::SessionStart(SessionStartEvent {
                session_id: sid.to_string(),
                proxy: state.name.clone(),
                ts: chrono::Utc::now().timestamp_millis(),
                client_name,
                client_version,
                client_platform,
            }));
    }
}
