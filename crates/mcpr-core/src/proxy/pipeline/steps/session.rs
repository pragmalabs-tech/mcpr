//! Session-lifecycle steps.
//!
//! Replaces `middleware::SessionTouchMiddleware`,
//! `middleware::DeleteSessionEndMiddleware`, and
//! `middleware::SessionStartMiddleware` as plain functions.

use axum::http::Method;
use axum::response::Response;

use crate::event::{ProxyEvent, SessionEndEvent, SessionStartEvent};
use crate::protocol::McpMethod;
use crate::protocol::session::{SessionState, SessionStore};

use crate::proxy::ProxyState;
use crate::proxy::pipeline::context::RequestContext;
use crate::proxy::pipeline::emit::normalize_platform;

/// Request-phase: bump `last_seen`, and flip `Initialized → Active` when
/// the client sends `notifications/initialized`. No-op when no session
/// id is on the request.
pub async fn touch(state: &ProxyState, ctx: &RequestContext) {
    let Some(sid) = ctx.session_id.as_deref() else {
        return;
    };
    state.sessions.touch(sid).await;
    if ctx.mcp_method == Some(McpMethod::Initialized) {
        state.sessions.update_state(sid, SessionState::Active).await;
    }
}

/// Request-phase: on `DELETE` with an `mcp-session-id` header, emit
/// `SessionEnd` and remove the session. Returns `None` (doesn't short-
/// circuit) — mcpr still forwards the DELETE to upstream.
pub async fn maybe_handle_delete(state: &ProxyState, ctx: &RequestContext) -> Option<Response> {
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

/// Response-phase: on a successful `initialize` response, create the
/// session in the store, record client info, and emit SessionStart.
pub async fn maybe_record_start(
    state: &ProxyState,
    ctx: &RequestContext,
    method: &McpMethod,
    status: u16,
) {
    if *method != McpMethod::Initialize || status >= 400 {
        return;
    }
    let Some(sid) = ctx.session_id.as_deref() else {
        return;
    };

    state.sessions.create(sid).await;
    state
        .sessions
        .update_state(sid, SessionState::Initialized)
        .await;

    let (client_name, client_version, client_platform) =
        if let Some(info) = ctx.client_info_from_init.clone() {
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
