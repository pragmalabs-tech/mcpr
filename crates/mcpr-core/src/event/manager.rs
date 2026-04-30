//! `EventManager` — collects registered sinks and starts the bus.
//!
//! ```rust,ignore
//! let mut manager = EventManager::new();
//! manager.register(Box::new(StderrSink::new(fmt)));
//! manager.register(Box::new(SqliteSink::new(store, "proxy-name")));
//! let handle = manager.start();
//! let bus = handle.bus.clone();
//! // … use bus.emit(event) from the proxy hot path …
//! handle.shutdown().await;
//! ```

use super::bus::{self, EventBusHandle};
use super::sink::EventSink;

/// Builder that holds the set of sinks to register before the bus starts.
#[derive(Default)]
pub struct EventManager {
    sinks: Vec<Box<dyn EventSink>>,
}

impl EventManager {
    pub fn new() -> Self {
        Self { sinks: Vec::new() }
    }

    /// Register a sink. Sinks receive events in the order they were registered.
    pub fn register(&mut self, sink: Box<dyn EventSink>) -> &mut Self {
        self.sinks.push(sink);
        self
    }

    /// Number of sinks currently registered.
    pub fn len(&self) -> usize {
        self.sinks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sinks.is_empty()
    }

    // Spawn an os thread that handle the event and not try to use thread of tokio
    pub fn start(self) -> EventBusHandle {
        bus::spawn(self.sinks)
    }
}
