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

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use std::sync::Arc;
    use std::time::{Duration, Instant};
    use tokio::sync::RwLock;

    use super::*;
    use crate::protocol::schema_manager::{MemorySchemaStore, SchemaManager};
    use crate::protocol::session::{MemorySessionStore, SessionInfo};
    use crate::proxy::forwarding::UpstreamClient;
    use crate::proxy::{CspConfig, RewriteConfig, new_shared_health};

    fn test_state() -> ProxyState {
        ProxyState {
            name: "t".into(),
            mcp_upstream: "http://u".into(),
            upstream: UpstreamClient {
                http_client: reqwest::Client::builder().build().unwrap(),
                semaphore: Arc::new(tokio::sync::Semaphore::new(1)),
                request_timeout: Duration::from_secs(1),
            },
            max_request_body: 1024,
            max_response_body: 1024,
            rewrite_config: Arc::new(RwLock::new(RewriteConfig {
                proxy_url: "http://p".into(),
                proxy_domain: "p".into(),
                mcp_upstream: "http://u".into(),
                csp: CspConfig::default(),
            })),
            widget_source: None,
            sessions: MemorySessionStore::new(),
            schema_manager: Arc::new(SchemaManager::new("t", MemorySchemaStore::new())),
            health: new_shared_health(),
            event_bus: crate::event::EventManager::new().start().bus,
        }
    }

    fn ctx_with(method: Option<McpMethod>, session_id: Option<&str>) -> RequestContext {
        RequestContext {
            start: Instant::now(),
            http_method: Method::POST,
            path: "/mcp".into(),
            request_size: 0,
            wants_sse: false,
            session_id: session_id.map(String::from),
            jsonrpc: None,
            mcp_method: method,
            mcp_method_str: None,
            tool: None,
            is_batch: false,
            client_info_from_init: None,
            client_name: None,
            client_version: None,
            tags: Vec::new(),
        }
    }

    #[tokio::test]
    async fn session_touch__initialized_transitions_to_active() {
        let state = test_state();
        state.sessions.create("sess-1").await;
        // Initialize first so the session reaches the `Initialized` state —
        // that's the one that transitions to Active.
        state
            .sessions
            .update_state("sess-1", SessionState::Initialized)
            .await;

        let mut ctx = ctx_with(Some(McpMethod::Initialized), Some("sess-1"));
        assert!(
            SessionTouchMiddleware
                .on_request(&state, &mut ctx)
                .await
                .is_none()
        );

        let info: SessionInfo = state.sessions.get("sess-1").await.unwrap();
        assert_eq!(info.state, SessionState::Active);
    }
}

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
