//! Proxy event pipeline — types, sink trait, bus, and manager.
//!
//! ## Layout
//!
//! - [`types`]: [`ProxyEvent`] enum and its per-variant payload structs.
//! - [`sink`]: [`EventSink`] trait that consumers implement.
//! - [`bus`]: [`EventBus`] (emit handle) and [`EventBusHandle`] (shutdown).
//! - [`manager`]: [`EventManager`] — builder that registers sinks and starts
//!   the bus.
//!
//! Typical wiring:
//!
//! ```rust,ignore
//! use mcpr_core::event::{EventManager, EventSink};
//!
//! let mut manager = EventManager::new();
//! manager.register(Box::new(my_sink));
//! let handle = manager.start();
//! let bus = handle.bus.clone();
//! // … bus.emit(event) from the proxy hot path …
//! handle.shutdown().await;
//! ```

pub mod bus;
pub mod manager;
pub mod sink;
pub mod types;

pub use bus::{EventBus, EventBusHandle};
pub use manager::EventManager;
pub use sink::{EventSink, NoopSink};
pub use types::ProxyEvent;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    use crate::protocol::Request;
    use crate::protocol::mcp::{
        ClientMethod, JsonRpcRequest, JsonRpcVersion, RequestId, ToolsMethod,
    };

    struct MemorySink {
        events: Arc<Mutex<Vec<ProxyEvent>>>,
        flush_count: Arc<Mutex<u32>>,
    }

    impl MemorySink {
        #[allow(clippy::type_complexity)]
        fn new() -> (Self, Arc<Mutex<Vec<ProxyEvent>>>, Arc<Mutex<u32>>) {
            let events = Arc::new(Mutex::new(Vec::new()));
            let flush_count = Arc::new(Mutex::new(0u32));
            (
                Self {
                    events: events.clone(),
                    flush_count: flush_count.clone(),
                },
                events,
                flush_count,
            )
        }
    }

    impl EventSink for MemorySink {
        fn on_event(&self, event: &ProxyEvent) {
            self.events.lock().unwrap().push(event.clone());
        }
        fn flush(&self) {
            *self.flush_count.lock().unwrap() += 1;
        }
        fn name(&self) -> &'static str {
            "memory"
        }
    }

    fn test_request() -> ProxyEvent {
        let parts = axum::http::Request::builder()
            .method("POST")
            .uri("/")
            .body(())
            .unwrap()
            .into_parts()
            .0;
        ProxyEvent::Request(Arc::new(Request::Mcp(
            parts,
            JsonRpcRequest {
                jsonrpc: JsonRpcVersion,
                id: RequestId::Number(1),
                method: ClientMethod::Tools(ToolsMethod::List),
                params: None,
            },
        )))
    }

    fn start_with(sinks: Vec<Box<dyn EventSink>>) -> EventBusHandle {
        let mut mgr = EventManager::new();
        for s in sinks {
            mgr.register(s);
        }
        mgr.start()
    }

    #[tokio::test]
    async fn routes_to_single_sink() {
        let (sink, events, _) = MemorySink::new();
        let handle = start_with(vec![Box::new(sink)]);

        handle.bus.emit(test_request());
        handle.bus.emit(test_request());

        handle.shutdown().await;

        let events = events.lock().unwrap();
        assert_eq!(events.len(), 2);
    }

    #[tokio::test]
    async fn routes_to_multiple_sinks() {
        let (sink1, events1, _) = MemorySink::new();
        let (sink2, events2, _) = MemorySink::new();
        let handle = start_with(vec![Box::new(sink1), Box::new(sink2)]);

        handle.bus.emit(test_request());

        handle.shutdown().await;

        assert_eq!(events1.lock().unwrap().len(), 1);
        assert_eq!(events2.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn flushes_on_shutdown() {
        let (sink, _, flush_count) = MemorySink::new();
        let handle = start_with(vec![Box::new(sink)]);

        handle.bus.emit(test_request());
        handle.shutdown().await;

        assert_eq!(*flush_count.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn drains_channel_on_shutdown() {
        let (sink, events, _) = MemorySink::new();
        let handle = start_with(vec![Box::new(sink)]);

        for _ in 0..100 {
            handle.bus.emit(test_request());
        }

        handle.shutdown().await;
        assert_eq!(events.lock().unwrap().len(), 100);
    }

    #[tokio::test]
    async fn works_with_no_sinks() {
        let handle = EventManager::new().start();
        handle.bus.emit(test_request());
        handle.shutdown().await;
    }
}
