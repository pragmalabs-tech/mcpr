//! Proxy event types carried through the event bus.
//!
//! [`ProxyEvent`] is the single event enum; sinks match on the variant to
//! decide what to do with each event. Variants hold their payload behind
//! `Arc` so fan-out to multiple sinks is a refcount bump rather than a
//! deep clone.

use std::sync::Arc;

use crate::protocol::{Request, Response, schema::ChangeSchema, session::SessionInfo};

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
}
