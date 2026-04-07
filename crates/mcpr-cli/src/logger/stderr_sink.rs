use std::io::Write;

use super::entry::LogEntry;
use super::sink::LogSink;
use crate::config::LogFormat;

/// Log sink that writes to stderr — the standard output for containers.
pub struct StderrSink {
    format: LogFormat,
}

impl StderrSink {
    pub fn new(format: LogFormat) -> Self {
        Self { format }
    }
}

impl LogSink for StderrSink {
    fn emit(&self, entry: &LogEntry) {
        let line = match self.format {
            LogFormat::Json => {
                // Serialize the full entry as JSON
                match serde_json::to_string(entry) {
                    Ok(json) => json,
                    Err(_) => return,
                }
            }
            LogFormat::Pretty => {
                let status = entry.status;
                let method = &entry.method;
                let path = &entry.path;
                let duration = entry
                    .duration_ms
                    .map(|ms| format!(" {ms}ms"))
                    .unwrap_or_default();
                let size = entry
                    .resp_size
                    .map(|b| {
                        if b >= 1024 {
                            format!(" {:.1}KB", b as f64 / 1024.0)
                        } else {
                            format!(" {b}B")
                        }
                    })
                    .unwrap_or_default();
                let mcp = entry
                    .mcp_method
                    .as_deref()
                    .map(|m| format!(" {m}"))
                    .unwrap_or_default();
                let detail = entry
                    .detail
                    .as_deref()
                    .map(|d| format!(" -> {d}"))
                    .unwrap_or_default();

                format!(
                    "{} {method} {status}{size}{duration}{mcp}{detail} {path}",
                    entry.timestamp
                )
            }
        };

        let stderr = std::io::stderr();
        let mut handle = stderr.lock();
        let _ = writeln!(handle, "{line}");
    }

    fn flush(&self) {
        let _ = std::io::stderr().flush();
    }
}
