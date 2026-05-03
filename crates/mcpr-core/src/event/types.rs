//! Proxy event types carried through the event bus.
//!
//! [`ProxyEvent`] is the single event enum; sinks match on the variant to
//! decide what to do with each event. Variants hold their payload behind
//! `Arc` so fan-out to multiple sinks is a refcount bump rather than a
//! deep clone.

use std::sync::Arc;

use chrono::{DateTime, Utc};

use crate::protocol::{Request, Response, schema::ChangeSchema, session::SessionInfo};

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
    Request(Arc<Request>),
    Response(Arc<Response>),
    Session(Arc<SessionInfo>),
    Schema(Arc<ChangeSchema>),
    Heartbeat(Arc<HeartbeatEvent>),
}
