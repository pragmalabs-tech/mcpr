mod entry;
pub mod file_sink;
mod sink;
mod tui_sink;

pub use entry::LogEntry;
pub use file_sink::{DEFAULT_MAX_FILES, FileSink, FileSinkConfig, Rotation, prefix_from_upstream};
pub use sink::LogSink;
pub use tui_sink::TuiSink;

use tokio::sync::mpsc;

const CHANNEL_CAPACITY: usize = 4096;
const BATCH_SIZE: usize = 256;
const FLUSH_INTERVAL_MS: u64 = 5000;

/// Routes log entries to multiple sinks via a bounded async channel.
///
/// The proxy hot path calls `emit()` which does a non-blocking channel send.
/// A background tokio task reads from the channel and fans out to all sinks.
#[derive(Clone)]
pub struct LogRouter {
    tx: mpsc::Sender<LogEntry>,
}

/// Handle returned by `LogRouter::new` to manage the background task.
pub struct LogRouterHandle {
    pub router: LogRouter,
    task: tokio::task::JoinHandle<()>,
    shutdown: mpsc::Sender<()>,
}

impl LogRouterHandle {
    /// Graceful shutdown: signals the background task and waits for it to drain.
    pub async fn shutdown(self) {
        let _ = self.shutdown.send(()).await;
        let _ = self.task.await;
    }
}

impl LogRouter {
    /// Create a new router with the given sinks.
    ///
    /// Spawns a background tokio task that reads entries and fans out to sinks.
    /// Returns a handle that owns both the router (for cloning into AppState)
    /// and the background task (for graceful shutdown).
    pub fn start(sinks: Vec<Box<dyn LogSink>>) -> LogRouterHandle {
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let (shutdown_tx, shutdown_rx) = mpsc::channel::<()>(1);

        let task = tokio::spawn(router_task(rx, shutdown_rx, sinks));

        LogRouterHandle {
            router: LogRouter { tx },
            task,
            shutdown: shutdown_tx,
        }
    }

    /// Send a log entry to all sinks. Non-blocking — drops the entry if the
    /// channel is full (never blocks the proxy request path).
    pub fn emit(&self, entry: LogEntry) {
        let _ = self.tx.try_send(entry);
    }
}

async fn router_task(
    mut rx: mpsc::Receiver<LogEntry>,
    mut shutdown_rx: mpsc::Receiver<()>,
    sinks: Vec<Box<dyn LogSink>>,
) {
    let mut batch = Vec::with_capacity(BATCH_SIZE);
    let mut flush_interval =
        tokio::time::interval(tokio::time::Duration::from_millis(FLUSH_INTERVAL_MS));
    flush_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            Some(entry) = rx.recv() => {
                batch.push(entry);

                // Drain more entries if available (non-blocking)
                while batch.len() < BATCH_SIZE {
                    match rx.try_recv() {
                        Ok(entry) => batch.push(entry),
                        Err(_) => break,
                    }
                }

                // Dispatch the batch
                dispatch_batch(&sinks, &batch);
                batch.clear();
            }
            _ = flush_interval.tick() => {
                // Periodic flush for sinks with internal buffers
                for sink in &sinks {
                    sink.flush();
                }
            }
            _ = shutdown_rx.recv() => {
                // Drain remaining entries
                while let Ok(entry) = rx.try_recv() {
                    batch.push(entry);
                }
                if !batch.is_empty() {
                    dispatch_batch(&sinks, &batch);
                    batch.clear();
                }
                // Final flush
                for sink in &sinks {
                    sink.flush();
                }
                return;
            }
        }
    }
}

fn dispatch_batch(sinks: &[Box<dyn LogSink>], batch: &[LogEntry]) {
    for sink in sinks {
        if batch.len() == 1 {
            sink.emit(&batch[0]);
        } else {
            sink.emit_batch(batch);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// Test sink that records entries in memory.
    struct MemorySink {
        entries: Arc<Mutex<Vec<LogEntry>>>,
        flush_count: Arc<Mutex<u32>>,
    }

    impl MemorySink {
        fn new() -> (Self, Arc<Mutex<Vec<LogEntry>>>, Arc<Mutex<u32>>) {
            let entries = Arc::new(Mutex::new(Vec::new()));
            let flush_count = Arc::new(Mutex::new(0u32));
            (
                Self {
                    entries: entries.clone(),
                    flush_count: flush_count.clone(),
                },
                entries,
                flush_count,
            )
        }
    }

    impl LogSink for MemorySink {
        fn emit(&self, entry: &LogEntry) {
            self.entries.lock().unwrap().push(entry.clone());
        }

        fn flush(&self) {
            *self.flush_count.lock().unwrap() += 1;
        }
    }

    #[tokio::test]
    async fn routes_to_single_sink() {
        let (sink, entries, _) = MemorySink::new();
        let handle = LogRouter::start(vec![Box::new(sink)]);

        handle
            .router
            .emit(LogEntry::new("POST", "/mcp", 200, "test"));
        handle
            .router
            .emit(LogEntry::new("GET", "/health", 200, "test"));

        handle.shutdown().await;

        let entries = entries.lock().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].method, "POST");
        assert_eq!(entries[1].method, "GET");
    }

    #[tokio::test]
    async fn routes_to_multiple_sinks() {
        let (sink1, entries1, _) = MemorySink::new();
        let (sink2, entries2, _) = MemorySink::new();
        let handle = LogRouter::start(vec![Box::new(sink1), Box::new(sink2)]);

        handle
            .router
            .emit(LogEntry::new("POST", "/mcp", 200, "test"));

        handle.shutdown().await;

        assert_eq!(entries1.lock().unwrap().len(), 1);
        assert_eq!(entries2.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn flushes_on_shutdown() {
        let (sink, _, flush_count) = MemorySink::new();
        let handle = LogRouter::start(vec![Box::new(sink)]);

        handle
            .router
            .emit(LogEntry::new("POST", "/mcp", 200, "test"));
        handle.shutdown().await;

        assert_eq!(*flush_count.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn drains_channel_on_shutdown() {
        let (sink, entries, _) = MemorySink::new();
        let handle = LogRouter::start(vec![Box::new(sink)]);

        // Emit several entries quickly
        for i in 0..100 {
            handle
                .router
                .emit(LogEntry::new("POST", &format!("/path/{i}"), 200, "test"));
        }

        handle.shutdown().await;

        // All entries should have been drained
        assert_eq!(entries.lock().unwrap().len(), 100);
    }

    #[tokio::test]
    async fn clone_router_shares_channel() {
        let (sink, entries, _) = MemorySink::new();
        let handle = LogRouter::start(vec![Box::new(sink)]);

        let router_clone = handle.router.clone();
        handle.router.emit(LogEntry::new("POST", "/a", 200, "test"));
        router_clone.emit(LogEntry::new("GET", "/b", 200, "test"));

        handle.shutdown().await;

        let entries = entries.lock().unwrap();
        assert_eq!(entries.len(), 2);
    }

    #[tokio::test]
    async fn works_with_no_sinks() {
        let handle = LogRouter::start(vec![]);
        handle
            .router
            .emit(LogEntry::new("POST", "/mcp", 200, "test"));
        handle.shutdown().await;
        // Should not panic
    }
}
