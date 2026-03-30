use super::entry::LogEntry;

/// Trait for log output backends.
///
/// Implement this to add new log destinations (file, database, metrics, etc.).
/// Sinks receive cloned `LogEntry` values from the `LogRouter` background task.
pub trait LogSink: Send + Sync + 'static {
    /// Process a single log entry. Called from the router's background task.
    fn emit(&self, entry: &LogEntry);

    /// Flush any buffered data. Called during graceful shutdown.
    fn flush(&self) {}
}
