//! SQLite event sink — writes proxy events to the local SQLite store.
//!
//! Converts `ProxyEvent` variants into store operations:
//! - `Request` → insert into `requests` table + update session counters
//! - `SessionStart` → insert into `sessions` table
//! - `SessionEnd` → update `ended_at` on the session
//! - `Heartbeat` → ignored (not stored locally)

use mcpr_core::event::{EventSink, ProxyEvent};

use super::engine::Store;
use super::event::{
    RequestEvent as StoreRequestEvent, RequestStatus, SessionEvent as StoreSessionEvent, StoreEvent,
};

/// Event sink that writes to the SQLite store.
///
/// Wraps the existing `Store` and converts `ProxyEvent` → `StoreEvent`.
pub struct SqliteSink {
    store: Store,
}

impl SqliteSink {
    pub fn new(store: Store) -> Self {
        Self { store }
    }

    /// Shutdown the underlying store (flush pending writes).
    pub fn shutdown(&mut self) {
        self.store.shutdown();
    }
}

impl EventSink for SqliteSink {
    fn on_event(&self, event: &ProxyEvent) {
        match event {
            ProxyEvent::Request(e) => {
                let status = if e.error_code.is_some() || e.status >= 500 {
                    RequestStatus::Error
                } else {
                    RequestStatus::Ok
                };

                self.store.record(StoreEvent::Request(StoreRequestEvent {
                    request_id: e.id.clone(),
                    ts: e.ts,
                    proxy: e.proxy.clone(),
                    session_id: e.session_id.clone(),
                    method: e.mcp_method.clone().unwrap_or_else(|| e.method.clone()),
                    tool: e.tool.clone(),
                    latency_us: e.latency_us as i64,
                    status,
                    error_code: e.error_code.clone(),
                    error_msg: e.error_msg.clone(),
                    bytes_in: e.request_size.map(|s| s as i64),
                    bytes_out: e.response_size.map(|s| s as i64),
                }));
            }
            ProxyEvent::SessionStart(e) => {
                self.store.record(StoreEvent::Session(StoreSessionEvent {
                    session_id: e.session_id.clone(),
                    proxy: e.proxy.clone(),
                    started_at: e.ts,
                    client_name: e.client_name.clone(),
                    client_version: e.client_version.clone(),
                    client_platform: e.client_platform.clone(),
                }));
            }
            ProxyEvent::SessionEnd(e) => {
                self.store.record(StoreEvent::SessionClosed {
                    session_id: e.session_id.clone(),
                    ended_at: e.ts,
                });
            }
            ProxyEvent::Heartbeat(_) => {
                // Heartbeats are not stored locally.
            }
            ProxyEvent::SchemaCapture(e) => {
                self.store.record(StoreEvent::SchemaCapture(
                    super::event::SchemaCaptureEvent {
                        ts: e.ts,
                        proxy: e.proxy.clone(),
                        upstream_url: e.upstream_url.clone(),
                        method: e.method.clone(),
                        payload: e.payload.clone(),
                        page_status: e.page_status.clone(),
                    },
                ));
            }
            ProxyEvent::SchemaStale(e) => {
                self.store.record(StoreEvent::SchemaStale {
                    proxy: e.proxy.clone(),
                    upstream_url: e.upstream_url.clone(),
                    method: e.method.clone(),
                    ts: e.ts,
                });
            }
            ProxyEvent::SchemaVersionCreated(_) => {
                // Not stored locally in this step — the SchemaManager owns
                // version persistence via its `SchemaStore`. A later step
                // adds a sqlite-backed `SchemaStore` impl.
            }
        }
    }

    fn name(&self) -> &'static str {
        "sqlite"
    }
}
