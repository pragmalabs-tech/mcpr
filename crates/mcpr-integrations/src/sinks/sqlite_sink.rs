//! SQLite event sink — writes proxy events to the local SQLite store.
//!
//! Converts `ProxyEvent` variants into store operations:
//! - `Request` → insert into `requests` table + update session counters
//! - `SessionStart` → insert into `sessions` table
//! - `SessionEnd` → update `ended_at` on the session
//! - `Heartbeat` → ignored (not stored locally)

use mcpr_core::event::{EventSink, ProxyEvent, SchemaVersionCreatedEvent};

use crate::store::engine::Store;
use crate::store::event::{
    RequestEvent as StoreRequestEvent, RequestStatus, SchemaVersionEvent,
    SessionEvent as StoreSessionEvent, StoreEvent,
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
                    resource_uri: e.resource_uri.clone(),
                    prompt_name: e.prompt_name.clone(),
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
            ProxyEvent::SchemaVersionCreated(e) => {
                self.store.record(map_schema_version(e));
            }
        }
    }

    fn name(&self) -> &'static str {
        "sqlite"
    }
}

fn map_schema_version(e: &SchemaVersionCreatedEvent) -> StoreEvent {
    StoreEvent::SchemaVersion(SchemaVersionEvent {
        ts: e.ts,
        proxy: e.upstream_id.clone(),
        upstream_url: e.upstream_url.clone(),
        method: e.method.clone(),
        payload: e.payload.to_string(),
        content_hash: e.content_hash.clone(),
    })
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn map_schema_version__copies_fields_correctly() {
        let event = SchemaVersionCreatedEvent {
            ts: 1_700_000_000_000,
            upstream_id: "api".into(),
            upstream_url: "http://localhost:9000".into(),
            method: "tools/list".into(),
            version: 3,
            version_id: "abc123def4567890".into(),
            content_hash: "abc123def4567890cafebabe".into(),
            payload: json!({"tools": [{"name": "search"}]}),
        };

        let StoreEvent::SchemaVersion(sv) = map_schema_version(&event) else {
            panic!("expected StoreEvent::SchemaVersion");
        };
        assert_eq!(sv.ts, 1_700_000_000_000);
        assert_eq!(sv.proxy, "api");
        assert_eq!(sv.upstream_url, "http://localhost:9000");
        assert_eq!(sv.method, "tools/list");
        assert_eq!(sv.content_hash, "abc123def4567890cafebabe");
        assert!(sv.payload.contains("search"));
    }

    #[test]
    fn map_schema_version__upstream_id_maps_to_proxy_column() {
        let event = SchemaVersionCreatedEvent {
            ts: 0,
            upstream_id: "proxy-alpha".into(),
            upstream_url: "".into(),
            method: "initialize".into(),
            version: 1,
            version_id: "0".into(),
            content_hash: "0".into(),
            payload: json!({}),
        };
        let StoreEvent::SchemaVersion(sv) = map_schema_version(&event) else {
            panic!();
        };
        assert_eq!(sv.proxy, "proxy-alpha");
    }
}
