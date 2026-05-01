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

use async_trait::async_trait;
use axum::http::Method;

use crate::{
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
#[derive(Clone, Debug, Default)]
pub struct RequestContext {
    inner: Arc<RequestContextInner>,
}

/// Underlying per-request state. Field access on [`RequestContext`]
/// goes through `Deref` to this struct, so call sites stay ergonomic.
#[derive(Debug, Default)]
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
    /// Proxy-internal request id. UUID minted at pipeline entry, used
    /// to correlate logs and metrics across stages and the upstream
    /// call. Distinct from any MCP `RequestId`, which is client-supplied.
    pub request_id: String,
    /// Per-stage timing recorder. Stages call `track_start` /
    /// `track_end` to mark spans; the pipeline can dump it after the
    /// response comes back to attribute latency to specific stages.
    pub timer: Timer,
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
    /// Build a context from a parsed `Request`. HTTP requests carry no
    /// MCP method, so the method map is empty.
    pub fn from_request(request: &Request) -> Self {
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
                    request_id: new_request_id(),
                    timer: Timer::new(),
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
                request_id: new_request_id(),
                timer: Timer::new(),
            },
            Request::Http(http) => RequestContextInner {
                client_methods: HashMap::new(),
                session_id: session_id_from_headers(http.headers()),
                initialize: None,
                is_session_close: http.method() == Method::DELETE,
                request_id: new_request_id(),
                timer: Timer::new(),
            },
        };
        inner.into()
    }

    pub fn get_method(&self, request_id: &RequestId) -> Option<&ClientMethod> {
        self.client_methods.get(request_id)
    }
}

fn new_request_id() -> String {
    uuid::Uuid::new_v4().to_string()
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
