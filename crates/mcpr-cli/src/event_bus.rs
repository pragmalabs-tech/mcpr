//! Event bus — routes proxy events to registered sinks.
//!
//! The proxy hot path calls `emit()` (non-blocking channel send).
//! A background tokio task reads events and fans out to all sinks.
//!
//! ```text
//! Proxy hot path ──► EventBus::emit(ProxyEvent) ──► [StderrSink, SqliteSink, CloudSink]
//! ```

use mcpr_core::event::{EventSink, ProxyEvent};
use tokio::sync::mpsc;

const CHANNEL_CAPACITY: usize = 10_000;
const BATCH_SIZE: usize = 256;
const FLUSH_INTERVAL_MS: u64 = 5000;

/// Routes proxy events to multiple sinks via a bounded async channel.
///
/// The proxy hot path calls `emit()` which does a non-blocking channel send.
/// A background tokio task reads from the channel and fans out to all sinks.
#[derive(Clone)]
pub struct EventBus {
    tx: mpsc::Sender<ProxyEvent>,
}

/// Handle returned by `EventBus::start` to manage the background task.
pub struct EventBusHandle {
    pub bus: EventBus,
    task: tokio::task::JoinHandle<()>,
    shutdown: mpsc::Sender<()>,
}

impl EventBusHandle {
    /// Graceful shutdown: signals the background task and waits for it to drain.
    pub async fn shutdown(self) {
        let _ = self.shutdown.send(()).await;
        let _ = self.task.await;
    }
}

impl EventBus {
    /// Create a new event bus with the given sinks.
    ///
    /// Spawns a background tokio task that reads events and fans out to sinks.
    pub fn start(sinks: Vec<Box<dyn EventSink>>) -> EventBusHandle {
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let (shutdown_tx, shutdown_rx) = mpsc::channel::<()>(1);

        let task = tokio::spawn(bus_task(rx, shutdown_rx, sinks));

        EventBusHandle {
            bus: EventBus { tx },
            task,
            shutdown: shutdown_tx,
        }
    }

    /// Emit a proxy event. Non-blocking — drops the event if the channel is
    /// full (never blocks the proxy request path).
    pub fn emit(&self, event: ProxyEvent) {
        let _ = self.tx.try_send(event);
    }
}

async fn bus_task(
    mut rx: mpsc::Receiver<ProxyEvent>,
    mut shutdown_rx: mpsc::Receiver<()>,
    sinks: Vec<Box<dyn EventSink>>,
) {
    let mut batch = Vec::with_capacity(BATCH_SIZE);
    let mut flush_interval =
        tokio::time::interval(tokio::time::Duration::from_millis(FLUSH_INTERVAL_MS));
    flush_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            Some(event) = rx.recv() => {
                batch.push(event);

                // Drain more events if available (non-blocking)
                while batch.len() < BATCH_SIZE {
                    match rx.try_recv() {
                        Ok(event) => batch.push(event),
                        Err(_) => break,
                    }
                }

                dispatch_batch(&sinks, &batch);
                batch.clear();
            }
            _ = flush_interval.tick() => {
                for sink in &sinks {
                    sink.flush();
                }
            }
            _ = shutdown_rx.recv() => {
                // Drain remaining events
                while let Ok(event) = rx.try_recv() {
                    batch.push(event);
                }
                if !batch.is_empty() {
                    dispatch_batch(&sinks, &batch);
                    batch.clear();
                }
                for sink in &sinks {
                    sink.flush();
                }
                return;
            }
        }
    }
}

fn dispatch_batch(sinks: &[Box<dyn EventSink>], batch: &[ProxyEvent]) {
    for sink in sinks {
        if batch.len() == 1 {
            sink.on_event(&batch[0]);
        } else {
            sink.on_batch(batch);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mcpr_core::event::RequestEvent;
    use std::sync::{Arc, Mutex};

    struct MemorySink {
        events: Arc<Mutex<Vec<ProxyEvent>>>,
        flush_count: Arc<Mutex<u32>>,
    }

    impl MemorySink {
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

    fn test_request(note: &str) -> ProxyEvent {
        ProxyEvent::Request(RequestEvent {
            id: uuid::Uuid::new_v4().to_string(),
            ts: chrono::Utc::now().timestamp_millis(),
            proxy: "test".into(),
            session_id: None,
            method: "POST".into(),
            path: "/mcp".into(),
            mcp_method: Some("tools/call".into()),
            tool: Some("search".into()),
            status: 200,
            latency_ms: 42,
            upstream_ms: Some(40),
            request_size: Some(100),
            response_size: Some(200),
            error_code: None,
            error_msg: None,
            note: note.into(),
        })
    }

    #[tokio::test]
    async fn routes_to_single_sink() {
        let (sink, events, _) = MemorySink::new();
        let handle = EventBus::start(vec![Box::new(sink)]);

        handle.bus.emit(test_request("a"));
        handle.bus.emit(test_request("b"));

        handle.shutdown().await;

        let events = events.lock().unwrap();
        assert_eq!(events.len(), 2);
    }

    #[tokio::test]
    async fn routes_to_multiple_sinks() {
        let (sink1, events1, _) = MemorySink::new();
        let (sink2, events2, _) = MemorySink::new();
        let handle = EventBus::start(vec![Box::new(sink1), Box::new(sink2)]);

        handle.bus.emit(test_request("a"));

        handle.shutdown().await;

        assert_eq!(events1.lock().unwrap().len(), 1);
        assert_eq!(events2.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn flushes_on_shutdown() {
        let (sink, _, flush_count) = MemorySink::new();
        let handle = EventBus::start(vec![Box::new(sink)]);

        handle.bus.emit(test_request("a"));
        handle.shutdown().await;

        assert_eq!(*flush_count.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn drains_channel_on_shutdown() {
        let (sink, events, _) = MemorySink::new();
        let handle = EventBus::start(vec![Box::new(sink)]);

        for _ in 0..100 {
            handle.bus.emit(test_request("a"));
        }

        handle.shutdown().await;
        assert_eq!(events.lock().unwrap().len(), 100);
    }

    #[tokio::test]
    async fn works_with_no_sinks() {
        let handle = EventBus::start(vec![]);
        handle.bus.emit(test_request("a"));
        handle.shutdown().await;
    }
}
