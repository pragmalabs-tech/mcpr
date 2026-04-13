//! Stderr event sink — prints proxy events to stderr for real-time visibility.
//!
//! Used in both daemon and foreground modes. Docker/k8s scrape stderr.

use std::io::Write;

use mcpr_core::event::{EventSink, ProxyEvent};

use crate::config::LogFormat;

/// Sink that prints proxy events to stderr.
pub struct StderrSink {
    format: LogFormat,
}

impl StderrSink {
    pub fn new(format: LogFormat) -> Self {
        Self { format }
    }
}

impl EventSink for StderrSink {
    fn on_event(&self, event: &ProxyEvent) {
        // Only print request events to stderr (the console log line).
        let ProxyEvent::Request(e) = event else {
            return;
        };

        let line = match self.format {
            LogFormat::Json => match serde_json::to_string(event) {
                Ok(json) => json,
                Err(_) => return,
            },
            LogFormat::Pretty => {
                let status = e.status;
                let method = &e.method;
                let path = &e.path;
                let duration = if e.latency_us >= 1_000 {
                    format!(" {:.2}ms", e.latency_us as f64 / 1_000.0)
                } else {
                    format!(" {}μs", e.latency_us)
                };
                let size = e
                    .response_size
                    .map(|b| {
                        if b >= 1024 {
                            format!(" {:.1}KB", b as f64 / 1024.0)
                        } else {
                            format!(" {b}B")
                        }
                    })
                    .unwrap_or_default();
                let mcp = e
                    .mcp_method
                    .as_deref()
                    .map(|m| format!(" {m}"))
                    .unwrap_or_default();
                let detail = e
                    .tool
                    .as_deref()
                    .map(|d| format!(" -> {d}"))
                    .unwrap_or_default();

                let ts = chrono::DateTime::from_timestamp_millis(e.ts)
                    .map(|dt| {
                        dt.with_timezone(&chrono::Local)
                            .format("%H:%M:%S")
                            .to_string()
                    })
                    .unwrap_or_default();

                format!("{ts} {method} {status}{size}{duration}{mcp}{detail} {path}")
            }
        };

        let stderr = std::io::stderr();
        let mut handle = stderr.lock();
        let _ = writeln!(handle, "{line}");
    }

    fn flush(&self) {
        let _ = std::io::stderr().flush();
    }

    fn name(&self) -> &'static str {
        "stderr"
    }
}
