//! Stage traits — pre/post hooks around the router. Each stage can
//! mutate the value, leave it untouched, or fail the pipeline by
//! returning an error.
//!
//! Both stage kinds receive a [`RequestContext`] alongside the value:
//! a per-request bundle built once at pipeline entry from the parsed
//! [`Request`]. Stages that need to correlate the response back to the
//! originating method, session, or initialize call read from it; stages
//! that don't ignore it.

use std::collections::HashMap;
use std::ops::Deref;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use axum::http::Method;

use crate::{
    auth::{self, AuthRequest},
    protocol::{
        Request, Response,
        mcp::{ClientInfo, ClientMethod, LifecycleMethod, RequestId},
        session::{SessionId, session_id_from_headers},
    },
    proxy2::state::ProxyState,
    timer::Timer,
};

/// Per-request metadata threaded through both stage chains. Cheap to
/// clone (one `Arc` bump) and passed by value to stages.
///
/// The fields live on [`RequestContextInner`] behind a shared `Arc` so
/// stages can hold or move the context without re-allocating the
/// `client_methods` map or duplicating the timer.
#[derive(Clone, Default)]
pub struct RequestContext {
    inner: Arc<RequestContextInner>,
}

/// Underlying per-request state. Field access on [`RequestContext`]
/// goes through `Deref` to this struct, so call sites stay ergonomic.
pub struct RequestContextInner {
    /// `RequestId → ClientMethod` map. 1 entry for single MCP requests,
    /// N entries for batches, empty for HTTP.
    pub client_methods: HashMap<RequestId, ClientMethod>,
    /// `mcp-session-id` from the inbound request headers. `None` for the
    /// `initialize` call (server creates the id in the response) or for
    /// raw HTTP without a session header.
    pub session_id: Option<SessionId>,
    /// `(request_id, client_info)` if the inbound was an `initialize`
    /// request. The response stage uses this to register the new session
    /// with the real client metadata at the moment the server returns
    /// `mcp-session-id`.
    pub initialize: Option<(RequestId, ClientInfo)>,
    /// Inbound was an HTTP `DELETE` carrying a session id - the spec's
    /// session-close signal. Triggers `end_session` when its response
    /// comes back.
    pub is_session_close: bool,
    /// Correlation key for the transaction. Source depends on the
    /// inbound request:
    ///
    /// - `Request::Mcp` -> JSON-RPC `id` stringified.
    /// - `Request::McpBatch` (legacy) -> first rpc's JSON-RPC `id`.
    /// - `Request::Http` -> fresh UUID v4 minted at pipeline entry.
    pub request_id: String,
    /// Snapshot of the parsed inbound request as it entered the
    /// pipeline. Stashed here so the response stage can build a
    /// consolidated `RequestEvent` carrying both halves of the
    /// transaction. `None` only on the `Default` path used by tests
    /// that don't go through `with_timer`.
    pub request: Option<Arc<Request>>,
    /// Wall-clock instant captured at pipeline entry. Response stages
    /// read this to compute end-to-end request latency.
    pub started_at: Instant,
    /// Per-stage timing recorder. Stages call `track_start` /
    /// `track_end` to mark spans; the pipeline can dump it after the
    /// response comes back to attribute latency to specific stages.
    pub timer: Timer,
    /// Credential observed on the inbound `Authorization` header.
    /// Populated by [`auth::parse_request_auth`] at pipeline entry.
    /// Defaults to a `None` credential when the header is absent.
    pub auth: AuthRequest,
}

impl Default for RequestContextInner {
    fn default() -> Self {
        Self {
            client_methods: HashMap::new(),
            session_id: None,
            initialize: None,
            is_session_close: false,
            request_id: String::new(),
            request: None,
            started_at: Instant::now(),
            timer: Timer::default(),
            auth: AuthRequest::default(),
        }
    }
}

impl Deref for RequestContext {
    type Target = RequestContextInner;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl From<RequestContextInner> for RequestContext {
    fn from(inner: RequestContextInner) -> Self {
        Self {
            inner: Arc::new(inner),
        }
    }
}

impl RequestContext {
    /// Build a context from a parsed `Request` with a fresh timer. Use
    /// [`Self::with_timer`] when the caller already has a timer that
    /// should accumulate spans across pre/post-pipeline work.
    pub fn from_request(request: &Request) -> Self {
        Self::with_timer(request, Timer::new())
    }

    /// Build a context from a parsed `Request`, reusing the provided
    /// timer so spans tracked outside the pipeline (e.g. parse / encode
    /// in the HTTP entry point) land in the same dump as the stage spans.
    pub fn with_timer(request: &Request, timer: Timer) -> Self {
        let started_at = Instant::now();
        let request_arc = Arc::new(request.clone());
        let inner = match request {
            Request::Mcp(parts, rpc) => {
                let mut client_methods = HashMap::with_capacity(1);
                client_methods.insert(rpc.id.clone(), rpc.method.clone());
                let initialize = if matches!(
                    rpc.method,
                    ClientMethod::Lifecycle(LifecycleMethod::Initialize)
                ) {
                    rpc.parse_client_info().map(|ci| (rpc.id.clone(), ci))
                } else {
                    None
                };
                RequestContextInner {
                    client_methods,
                    session_id: session_id_from_headers(&parts.headers),
                    initialize,
                    is_session_close: false,
                    request_id: rpc.id.to_string(),
                    request: Some(request_arc),
                    started_at,
                    timer,
                    auth: auth::parse_request_auth(&parts.headers),
                }
            }
            Request::McpBatch(parts, rpcs) => RequestContextInner {
                client_methods: rpcs
                    .iter()
                    .map(|r| (r.id.clone(), r.method.clone()))
                    .collect(),
                session_id: session_id_from_headers(&parts.headers),
                // Batches can't carry initialize per spec - the lifecycle
                // request is single-shot. Skip the scan.
                initialize: None,
                is_session_close: false,
                // Legacy MCP batch (removed from spec in 2025-06-18). Use
                // the first rpc's id as the correlation key; sinks that
                // care about per-rpc detail still see the full batch in
                // `request`.
                request_id: rpcs.first().map(|r| r.id.to_string()).unwrap_or_default(),
                request: Some(request_arc),
                started_at,
                timer,
                auth: auth::parse_request_auth(&parts.headers),
            },
            Request::OAuth(oauth) => RequestContextInner {
                client_methods: HashMap::new(),
                session_id: session_id_from_headers(oauth.http.headers()),
                initialize: None,
                is_session_close: oauth.http.method() == Method::DELETE,
                request_id: uuid::Uuid::new_v4().to_string(),
                request: Some(request_arc),
                started_at,
                timer,
                auth: auth::parse_request_auth(oauth.http.headers()),
            },
            Request::Http(http) => RequestContextInner {
                client_methods: HashMap::new(),
                session_id: session_id_from_headers(http.headers()),
                initialize: None,
                is_session_close: http.method() == Method::DELETE,
                request_id: uuid::Uuid::new_v4().to_string(),
                request: Some(request_arc),
                started_at,
                timer,
                auth: auth::parse_request_auth(http.headers()),
            },
        };
        inner.into()
    }

    pub fn get_method(&self, request_id: &RequestId) -> Option<&ClientMethod> {
        self.client_methods.get(request_id)
    }
}

#[async_trait]
pub trait RequestStage: Send + Sync {
    /// Stable name used as the timer label in `process_with_timer` and
    /// for log/metric correlation. Method (not associated const) so the
    /// trait stays dyn-compatible for `Box<dyn RequestStage>` storage.
    fn name(&self) -> &'static str;

    async fn process(
        &self,
        request: Request,
        request_ctx: RequestContext,
        state: ProxyState,
    ) -> anyhow::Result<Request>;

    async fn process_with_timer(
        &self,
        request: Request,
        request_ctx: RequestContext,
        state: ProxyState,
    ) -> anyhow::Result<Request> {
        let timer_id = request_ctx.timer.track_start(self.name());
        let result = self.process(request, request_ctx.clone(), state).await;
        request_ctx.timer.track_end(timer_id);

        result
    }
}

#[async_trait]
pub trait ResponseStage: Send + Sync {
    /// Stable name used as the timer label in `process_with_timer` and
    /// for log/metric correlation. Method (not associated const) so the
    /// trait stays dyn-compatible for `Box<dyn ResponseStage>` storage.
    fn name(&self) -> &'static str;

    async fn process(
        &self,
        response: Response,
        request_ctx: RequestContext,
        state: ProxyState,
    ) -> anyhow::Result<Response>;

    async fn process_with_timer(
        &self,
        response: Response,
        request_ctx: RequestContext,
        state: ProxyState,
    ) -> anyhow::Result<Response> {
        let timer_id = request_ctx.timer.track_start(self.name());
        let result = self.process(response, request_ctx.clone(), state).await;
        request_ctx.timer.track_end(timer_id);

        result
    }
}
