//! Proxy event types carried through the event bus.
//!
//! [`ProxyEvent`] is the single event enum; sinks match on the variant to
//! decide what to do with each event.

use crate::protocol::{
    Request, Response,
    schema::ChangeSchema,
    session::SessionInfo,
};

/// All events flowing through the event bus.
///
/// Each variant represents a distinct lifecycle moment. Sinks match on
/// the variant to decide what to process.
#[derive(Clone)]
pub enum ProxyEvent {
    Request(Box<Request>),
    Response(Box<Response>),
    Session(Box<SessionInfo>),
    Schema(Box<ChangeSchema>),
}
