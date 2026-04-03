use crate::event::McprEvent;

/// Trait for event output backends.
///
/// Implementations:
/// - StdoutEmitter: JSON to stdout (default, ships day 1)
/// - CloudEmitter: HTTPS POST to cloud.mcpr.app (Phase 1, cloud sync)
pub trait EventEmitter: Send + Sync + 'static {
    /// Emit a single event. Must be non-blocking on the hot path.
    fn emit(&self, event: McprEvent);

    /// Flush buffered events. Called during graceful shutdown.
    fn flush(&self) {}
}

/// No-op emitter for when events are disabled.
pub struct NoopEmitter;

impl EventEmitter for NoopEmitter {
    fn emit(&self, _event: McprEvent) {}
}
