use super::entry::LogEntry;

/// Trait for log output backends.
///
/// Implement this to add new log destinations (file, database, metrics, etc.).
/// Sinks receive `LogEntry` values from the `LogRouter` background task.
///
/// For high-throughput scenarios, override `emit_batch()` to amortize
/// per-entry overhead (mutex acquisition, disk I/O, network calls).
pub trait LogSink: Send + Sync + 'static {
    /// Process a single log entry. Called from the router's background task.
    fn emit(&self, entry: &LogEntry);

    /// Process a batch of log entries. Default implementation calls `emit()`
    /// for each entry. Override for sinks that benefit from batching
    /// (e.g., file I/O, database inserts).
    fn emit_batch(&self, entries: &[LogEntry]) {
        for entry in entries {
            self.emit(entry);
        }
    }

    /// Flush any buffered data. Called during graceful shutdown.
    fn flush(&self) {}
}
