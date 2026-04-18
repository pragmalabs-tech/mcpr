//! Event bus — routes proxy events to registered sinks.
//!
//! The proxy hot path calls `emit()` (non-blocking channel send).
//! A background tokio task reads events and fans out to all sinks.
//!
//! ```text
//! Proxy hot path ──► EventBus::emit(ProxyEvent) ──► [StderrSink, SqliteSink, CloudSink]
//! ```
//!
//! Build a bus via [`EventManager`](super::EventManager) — it collects
//! sinks at startup and hands back an [`EventBusHandle`].

use super::sink::EventSink;
use super::types::ProxyEvent;
use tokio::sync::mpsc;

pub(super) const CHANNEL_CAPACITY: usize = 10_000;
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

/// Handle returned when the bus starts — use it to drive graceful shutdown
/// and to clone [`EventBus`] handles for emitters.
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
    /// Emit a proxy event. Non-blocking — drops the event if the channel is
    /// full (never blocks the proxy request path).
    pub fn emit(&self, event: ProxyEvent) {
        let _ = self.tx.try_send(event);
    }
}

/// Spawn the background dispatch task and return a live [`EventBusHandle`].
/// Callers should go through [`EventManager`](super::EventManager) rather
/// than calling this directly.
pub(super) fn spawn(sinks: Vec<Box<dyn EventSink>>) -> EventBusHandle {
    let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
    let (shutdown_tx, shutdown_rx) = mpsc::channel::<()>(1);

    let task = tokio::spawn(bus_task(rx, shutdown_rx, sinks));

    EventBusHandle {
        bus: EventBus { tx },
        task,
        shutdown: shutdown_tx,
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
