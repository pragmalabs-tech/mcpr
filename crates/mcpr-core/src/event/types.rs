//! Proxy event types carried through the event bus.
//!
//! [`ProxyEvent`] is the single event enum; sinks match on the variant to
//! decide what to do with each event. Variants hold their payload behind
//! `Arc` so fan-out to multiple sinks is a refcount bump rather than a
//! deep clone.

use std::sync::Arc;

use axum::http::{
    Method, StatusCode, Uri, request::Parts as RequestParts, response::Parts as ResponseParts,
};
use chrono::{DateTime, Utc};

use crate::protocol::{
    Request, Response,
    mcp::{JsonRpcRequest, JsonRpcResult},
    schema::ChangeSchema,
    session::SessionInfo,
};

/// Logging projection of [`Request`]. MCP variants carry the full
/// JSON-RPC envelope and inbound HTTP `Parts`; the HTTP variant keeps
/// only routing-level metadata so non-MCP traffic (HTML pages, SSE
/// streams, health probes) does not bloat sinks.
#[derive(Clone)]
pub enum LoggedRequest {
    Mcp(RequestParts, JsonRpcRequest),
    McpBatch(RequestParts, Vec<JsonRpcRequest>),
    Http {
        method: Method,
        uri: Uri,
        body_size: usize,
    },
}

impl From<&Request> for LoggedRequest {
    fn from(req: &Request) -> Self {
        match req {
            Request::Mcp(parts, rpc) => Self::Mcp(parts.clone(), rpc.clone()),
            Request::McpBatch(parts, rpcs) => Self::McpBatch(parts.clone(), rpcs.clone()),
            Request::Http(http) => Self::Http {
                method: http.method().clone(),
                uri: http.uri().clone(),
                body_size: http.body().len(),
            },
        }
    }
}

/// Logging projection of [`Response`]. Mirrors [`LoggedRequest`]: MCP
/// keeps the full JSON-RPC result and upstream `Parts`; HTTP carries
/// only the status and body size.
#[derive(Clone)]
pub enum LoggedResponse {
    Mcp(ResponseParts, JsonRpcResult),
    McpBatch(ResponseParts, Vec<JsonRpcResult>),
    Http {
        status: StatusCode,
        body_size: usize,
    },
}

impl From<&Response> for LoggedResponse {
    fn from(resp: &Response) -> Self {
        match resp {
            Response::Mcp(parts, result) => Self::Mcp(parts.clone(), result.clone()),
            Response::McpBatch(parts, results) => Self::McpBatch(parts.clone(), results.clone()),
            Response::Http(http) => Self::Http {
                status: http.status(),
                body_size: http.body().len(),
            },
        }
    }
}

/// One full request/response transaction, emitted once after the
/// response stage completes (`response: Some(...)`) or once on the
/// error path when the response stage never ran (`response: None`).
///
/// - `request_id` is the correlation key sinks should use to join the
///   request and response halves. Source depends on the variant:
///   MCP requests use the JSON-RPC `id` (stringified); HTTP requests
///   get a fresh UUID v4 minted at pipeline entry. MCP batch (legacy,
///   removed from spec in 2025-06-18) falls back to the first rpc's id.
/// - `request` and `response` are the logging projections of what the
///   pipeline saw. HTTP traffic is reduced to method/uri/status/size;
///   MCP traffic carries the full JSON-RPC envelope. `response` is
///   `None` for orphan transactions: parse-pass-but-pipeline-fail,
///   client disconnects mid-request, request stage errors, etc.
/// - `latency_us` / `upstream_us` are the headline numbers extracted
///   from the per-request timer; `spans` is the full span snapshot.
///   `upstream_us` is 0 when the router never ran (orphan).
/// - `ts` is captured at emit time so cloud-sink batching delay doesn't
///   skew analytics.
#[derive(Clone)]
pub struct RequestEvent {
    pub request_id: String,
    pub request: LoggedRequest,
    pub response: Option<LoggedResponse>,
    pub ts: DateTime<Utc>,
    pub latency_us: u64,
    pub upstream_us: u64,
    pub spans: Vec<(String, u64)>,
}

/// Periodic snapshot of a proxy's runtime status. Emitted on a fixed
/// cadence by the CLI host (not by the request pipeline) so the cloud
/// can answer "is this server up and where does it live?" without a
/// dedicated control-plane connection.
#[derive(Clone)]
pub struct HeartbeatEvent {
    pub mcp_status: String,
    pub tunnel_status: String,
    pub tunnel_address: Option<String>,
    pub upstream: String,
    pub export_port: u16,
    pub ts: DateTime<Utc>,
}

/// All events flowing through the event bus.
///
/// Each variant represents a distinct lifecycle moment. Sinks match on
/// the variant to decide what to process.
#[derive(Clone)]
pub enum ProxyEvent {
    Request(Arc<RequestEvent>),
    Session(Arc<SessionInfo>),
    Schema(Arc<ChangeSchema>),
    Heartbeat(Arc<HeartbeatEvent>),
}
