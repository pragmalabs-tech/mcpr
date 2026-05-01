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
/// clone; populated once by [`Self::from_request`] and read by stages.
#[derive(Clone, Debug, Default)]
pub struct RequestContext {
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

impl RequestContext {
    /// Build a context from a parsed `Request`. HTTP requests carry no
    /// MCP method, so the method map is empty.
    pub fn from_request(request: &Request) -> Self {
        match request {
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
                Self {
                    client_methods,
                    session_id: session_id_from_headers(&parts.headers),
                    initialize,
                    is_session_close: false,
                    request_id: new_request_id(),
                    timer: Timer::new(),
                }
            }
            Request::McpBatch(parts, rpcs) => Self {
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
            Request::Http(http) => Self {
                client_methods: HashMap::new(),
                session_id: session_id_from_headers(http.headers()),
                initialize: None,
                is_session_close: http.method() == Method::DELETE,
                request_id: new_request_id(),
                timer: Timer::new(),
            },
        }
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
    async fn process(
        &self,
        request: Request,
        request_ctx: RequestContext,
        state: ProxyState,
    ) -> anyhow::Result<Request>;
}

#[async_trait]
pub trait ResponseStage: Send + Sync {
    async fn process(
        &self,
        response: Response,
        request_ctx: RequestContext,
        state: ProxyState,
    ) -> anyhow::Result<Response>;
}
