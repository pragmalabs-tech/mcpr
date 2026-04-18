//! `EventSink` trait — how consumers plug into the event bus.

use super::types::ProxyEvent;

/// A sink that consumes proxy events from the event bus.
///
/// Register sinks via [`EventManager`](super::EventManager). The event bus
/// calls `on_event` for every event, and sinks filter by variant. Example:
///
/// ```rust,ignore
/// impl EventSink for PrometheusSink {
///     fn on_event(&self, event: &ProxyEvent) {
///         if let ProxyEvent::Request(e) = event {
///             self.request_counter.inc();
///             self.latency_histogram.observe(e.latency_us as f64);
///         }
///     }
///     fn name(&self) -> &'static str { "prometheus" }
/// }
/// ```
///
/// # Contract
///
/// - **`on_event` must not block.** If the sink needs I/O (HTTP, disk),
///   buffer internally and flush in `flush()` or a background thread.
/// - **`on_batch`** is called when multiple events are available. Override
///   for sinks that benefit from batching (SQL INSERT, HTTP POST).
/// - **`flush`** is called periodically (~5s) and on graceful shutdown.
pub trait EventSink: Send + Sync {
    /// Process a single event. Must not block.
    fn on_event(&self, event: &ProxyEvent);

    /// Process a batch of events. Default calls `on_event` for each.
    fn on_batch(&self, events: &[ProxyEvent]) {
        for event in events {
            self.on_event(event);
        }
    }

    /// Flush internal buffers to their destination.
    fn flush(&self) {}

    /// Human-readable sink name (for logging and debugging).
    fn name(&self) -> &'static str;
}

/// A no-op sink that discards all events. Used when no sinks are configured.
pub struct NoopSink;

impl EventSink for NoopSink {
    fn on_event(&self, _event: &ProxyEvent) {}
    fn name(&self) -> &'static str {
        "noop"
    }
}
