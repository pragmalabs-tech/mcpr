//! Proxy event types carried through the event bus.
//!
//! [`ProxyEvent`] is the single event enum; sinks match on the variant to
//! decide what to do with each event. Variants hold their payload behind
//! `Arc` so fan-out to multiple sinks is a refcount bump rather than a
//! deep clone.

use std::sync::Arc;

use chrono::{DateTime, Utc};

use crate::protocol::{Request, Response, schema::ChangeSchema, session::SessionInfo};

/// One full request/response transaction, emitted once after the
/// response stage completes (`response: Some(...)`) or once on the
/// error path when the response stage never ran (`response: None`).
///
/// - `request_id` is the correlation key sinks should use to join the
///   request and response halves. Source depends on the variant:
///   MCP requests use the JSON-RPC `id` (stringified); HTTP requests
///   get a fresh UUID v4 minted at pipeline entry. MCP batch (legacy,
///   removed from spec in 2025-06-18) falls back to the first rpc's id.
/// - `request` and `response` are the protocol-level payloads as the
///   pipeline saw them at intake and at upstream return. `response`
///   is `None` for orphan transactions: parse-pass-but-pipeline-fail,
///   client disconnects mid-request, request stage errors, etc.
/// - `latency_us` / `upstream_us` are the headline numbers extracted
///   from the per-request timer; `spans` is the full span snapshot.
///   `upstream_us` is 0 when the router never ran (orphan).
/// - `ts` is captured at emit time so cloud-sink batching delay doesn't
///   skew analytics.
#[derive(Clone)]
pub struct RequestEvent {
    pub request_id: String,
    pub request: Request,
    pub response: Option<Response>,
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
