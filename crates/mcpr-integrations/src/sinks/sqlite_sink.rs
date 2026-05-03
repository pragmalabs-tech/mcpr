//! SQLite event sink — refcount dispatcher onto the storage channel.
//!
//! Each `ProxyEvent` is wrapped in a [`StoreEvent`] together with two
//! pieces of context (`ts` stamped at sink time, `proxy` injected from
//! sink config) and forwarded to the [`Store`]. The sink does no
//! parsing or string allocation per event; the `Arc`s inside the
//! `ProxyEvent` flow through to the writer by refcount, where SQL
//! binding happens.

use std::sync::Arc;

use chrono::Utc;
use mcpr_core::event::{EventSink, ProxyEvent};

use crate::store::engine::Store;
use crate::store::event::StoreEvent;

/// Adapter from the proxy event bus to the storage writer.
pub struct SqliteSink {
    store: Store,
    proxy: Arc<str>,
}

impl SqliteSink {
    /// Build a sink for a given proxy name. The name tags every row the
    /// sink writes, so a shared database can hold data from multiple
    /// proxies without ambiguity.
    pub fn new(store: Store, proxy: impl Into<Arc<str>>) -> Self {
        Self {
            store,
            proxy: proxy.into(),
        }
    }

    /// Drain pending events and stop the writer thread.
    pub fn shutdown(&mut self) {
        self.store.shutdown();
    }
}

impl EventSink for SqliteSink {
    fn on_event(&self, event: &ProxyEvent) {
        self.store.record(StoreEvent {
            ts: Utc::now().timestamp_millis(),
            proxy: Arc::clone(&self.proxy),
            event: event.clone(),
        });
    }

    fn name(&self) -> &'static str {
        "sqlite"
    }
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;

    use std::sync::Arc;

    use chrono::Utc;
    use mcpr_core::event::types::{LoggedRequest, LoggedResponse, RequestEvent};
    use mcpr_core::protocol::mcp::{
        ClientMethod, JsonRpcRequest, JsonRpcResponse, JsonRpcResult, JsonRpcVersion, RequestId,
        ToolsMethod,
    };
    use serde_json::json;

    use crate::store::engine::StoreConfig;

    fn open_store() -> (Store, std::path::PathBuf, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("sink.db");
        let store = Store::open(StoreConfig {
            db_path: db_path.clone(),
            mcpr_version: "test".into(),
        })
        .unwrap();
        (store, db_path, dir)
    }

    fn rpc(id: i64, tool: &str) -> JsonRpcRequest {
        JsonRpcRequest {
            jsonrpc: JsonRpcVersion,
            id: RequestId::Number(id),
            method: ClientMethod::Tools(ToolsMethod::Call),
            params: Some(serde_json::Map::from_iter([("name".into(), json!(tool))])),
        }
    }

    fn mcp_request(sid: &str, rpc: JsonRpcRequest) -> LoggedRequest {
        let parts = http::Request::builder()
            .method("POST")
            .uri("/")
            .header("mcp-session-id", sid)
            .body(())
            .unwrap()
            .into_parts()
            .0;
        LoggedRequest::Mcp(parts, rpc)
    }

    fn ok_response(id: i64) -> LoggedResponse {
        let parts = http::Response::new(()).into_parts().0;
        LoggedResponse::Mcp(
            parts,
            JsonRpcResult::Response(JsonRpcResponse {
                jsonrpc: JsonRpcVersion,
                id: RequestId::Number(id),
                result: Some(json!({})),
            }),
        )
    }

    fn transaction(req: LoggedRequest, resp: LoggedResponse) -> ProxyEvent {
        ProxyEvent::Request(Arc::new(RequestEvent {
            request_id: "rid".into(),
            request: req,
            response: Some(resp),
            ts: Utc::now(),
            latency_us: 0,
            upstream_us: 0,
            spans: vec![],
        }))
    }

    #[test]
    fn on_event__forwards_request_to_store_with_proxy_tag() {
        let (store, db_path, _dir) = open_store();
        let mut sink = SqliteSink::new(store, "alpha");

        sink.on_event(&transaction(
            mcp_request("sess-1", rpc(1, "search")),
            ok_response(1),
        ));

        sink.shutdown();

        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let (proxy, tool): (String, String) = conn
            .query_row(
                "SELECT proxy, tool FROM requests WHERE request_id = '1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(proxy, "alpha");
        assert_eq!(tool, "search");
    }

    #[test]
    fn name__is_sqlite() {
        let (store, _path, _dir) = open_store();
        let sink = SqliteSink::new(store, "p");
        assert_eq!(sink.name(), "sqlite");
    }
}
