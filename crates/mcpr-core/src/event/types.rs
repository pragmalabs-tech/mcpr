//! Proxy event types carried through the event bus.
//!
//! [`ProxyEvent`] is the single event enum; sinks match on the variant to
//! decide what to do with each event. The `Request` and `Response`
//! variants wrap a small metadata struct around the protocol value so
//! downstream sinks have the proxy-internal request id, emission
//! timestamp, and response latency without re-deriving them.

use std::sync::Arc;

use chrono::{DateTime, Utc};

use crate::protocol::{Request, Response, schema::ChangeSchema, session::SessionInfo};

/// Request observation: the parsed request alongside the proxy-internal
/// `request_id` (for cross-event correlation) and the emission timestamp.
#[derive(Clone)]
pub struct RequestEvent {
    pub request: Request,
    pub request_id: String,
    pub ts: DateTime<Utc>,
}

/// Response observation: the parsed response, the matching `request_id`,
/// the round-trip latency captured at the response stage, and the
/// emission timestamp.
#[derive(Clone)]
pub struct ResponseEvent {
    pub response: Response,
    pub request_id: String,
    pub latency_us: u64,
    pub ts: DateTime<Utc>,
}

/// All events flowing through the event bus.
///
/// Each variant represents a distinct lifecycle moment. Sinks match on
/// the variant to decide what to process.
#[derive(Clone)]
pub enum ProxyEvent {
    Request(Arc<RequestEvent>),
    Response(Arc<ResponseEvent>),
    Session(Arc<SessionInfo>),
    Schema(Arc<ChangeSchema>),
}
