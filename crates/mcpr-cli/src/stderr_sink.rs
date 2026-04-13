//! Stderr event sink — prints proxy events to stderr for real-time visibility.
//!
//! Used in both daemon and foreground modes. Docker/k8s scrape stderr.

use std::io::Write;

use mcpr_core::event::{EventSink, ProxyEvent};

use crate::config::LogFormat;

/// Format latency in microseconds for stderr display.
///
/// - < 1ms: "200μs"
/// - ≥ 1ms: "4.20ms"
fn format_duration_us(us: u64) -> String {
    if us >= 1_000 {
        format!(" {:.2}ms", us as f64 / 1_000.0)
    } else {
        format!(" {}μs", us)
    }
}

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
                let duration = format_duration_us(e.latency_us);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_duration_sub_ms() {
        assert_eq!(format_duration_us(0), " 0μs");
        assert_eq!(format_duration_us(1), " 1μs");
        assert_eq!(format_duration_us(200), " 200μs");
        assert_eq!(format_duration_us(999), " 999μs");
    }

    #[test]
    fn format_duration_ms_range() {
        assert_eq!(format_duration_us(1_000), " 1.00ms");
        assert_eq!(format_duration_us(1_500), " 1.50ms");
        assert_eq!(format_duration_us(42_000), " 42.00ms");
        assert_eq!(format_duration_us(999_999), " 1000.00ms");
    }

    #[test]
    fn format_duration_large() {
        assert_eq!(format_duration_us(1_000_000), " 1000.00ms");
        assert_eq!(format_duration_us(5_000_000), " 5000.00ms");
    }

    #[test]
    fn format_duration_boundary() {
        // Exact boundary between μs and ms display
        assert_eq!(format_duration_us(999), " 999μs");
        assert_eq!(format_duration_us(1_000), " 1.00ms");
    }
}
