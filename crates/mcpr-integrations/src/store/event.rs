//! Storage channel envelope.
//!
//! [`StoreEvent`] is what the sink hands the writer through the mpsc
//! channel. It wraps the original [`ProxyEvent`] verbatim — the inner
//! `Arc<Request>` / `Arc<Response>` / `Arc<SessionInfo>` /
//! `Arc<ChangeSchema>` flow through to the writer by refcount only.
//!
//! Only two pieces of context get added at the sink:
//! - `ts`    — sink-stamped unix milliseconds, since the protocol types
//!             don't carry one of their own.
//! - `proxy` — sink-injected proxy name, since events are per-proxy and
//!             the protocol types don't know which proxy emitted them.
//!
//! All extraction (status, kind tag, item key, payload JSON, hash, …)
//! happens in the writer at SQL bind time, where the prepared statements
//! and the connection live.

use std::sync::Arc;

use mcpr_core::event::ProxyEvent;

/// One entry in the storage channel.
///
/// Cheap to clone (refcount bumps only). The writer drops `ts` / `proxy`
/// straight into the `requests` / `sessions` / `schema_*` tables, and
/// reads everything else off the inner `ProxyEvent`'s `Arc`s.
#[derive(Clone)]
pub struct StoreEvent {
    /// Unix milliseconds, stamped at the moment the sink received the event.
    pub ts: i64,

    /// Proxy name from sink config. Sink owns one `Arc<str>` and clones it
    /// into every event — no per-event string allocation.
    pub proxy: Arc<str>,

    /// The original event, with inner `Arc`s preserved.
    pub event: ProxyEvent,
}
