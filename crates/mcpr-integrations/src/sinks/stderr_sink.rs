//! Stderr event sink — prints proxy events to stderr for real-time visibility.
//!
//! Used in both daemon and foreground modes. Docker/k8s scrape stderr.

use std::io::Write;

use mcpr_core::event::{EventSink, ProxyEvent};
use mcpr_core::time::format_latency_us;

// ── Log format ──────────────────────────────────────────────────────────

/// Log output format for stderr.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LogFormat {
    #[default]
    Json,
    Pretty,
}

impl std::str::FromStr for LogFormat {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "json" => Ok(LogFormat::Json),
            "pretty" | "text" => Ok(LogFormat::Pretty),
            _ => Err(format!("unknown log format: {s} (expected: json, pretty)")),
        }
    }
}

impl std::fmt::Display for LogFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LogFormat::Json => write!(f, "json"),
            LogFormat::Pretty => write!(f, "pretty"),
        }
    }
}

// ── Sink ────────────────────────────────────────────────────────────────

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
                let duration = format_latency_us(e.latency_us as i64);
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

                format!("{ts} {method} {status}{size} {duration}{mcp}{detail} {path}")
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
#[allow(non_snake_case)]
mod tests {
    use super::*;
    use mcpr_core::event::RequestEvent;

    fn make_event(latency_us: u64) -> ProxyEvent {
        ProxyEvent::Request(Box::new(RequestEvent {
            id: "test".into(),
            ts: 1_700_000_000_000,
            proxy: "api".into(),
            session_id: None,
            method: "POST".into(),
            path: "/mcp".into(),
            mcp_method: Some("tools/call".into()),
            tool: Some("search".into()),
            status: 200,
            latency_us,
            upstream_us: None,
            request_size: Some(100),
            response_size: Some(200),
            error_code: None,
            error_msg: None,
            client_name: None,
            client_version: None,
            note: "test".into(),
        }))
    }

    #[test]
    fn stderr_sink__pretty_sub_ms_latency() {
        let sink = StderrSink::new(LogFormat::Pretty);
        let event = make_event(200);
        sink.on_event(&event);
    }

    #[test]
    fn stderr_sink__pretty_ms_latency() {
        let sink = StderrSink::new(LogFormat::Pretty);
        let event = make_event(4_200);
        sink.on_event(&event);
    }

    #[test]
    fn stderr_sink__pretty_seconds_latency() {
        let sink = StderrSink::new(LogFormat::Pretty);
        let event = make_event(1_500_000);
        sink.on_event(&event);
    }

    #[test]
    fn stderr_sink__json_contains_latency_us() {
        let event = make_event(200);
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"latency_us\":200"));
        assert!(!json.contains("latency_ms"));
    }

    #[test]
    fn log_format__parses_known_strings() {
        assert_eq!("json".parse::<LogFormat>().unwrap(), LogFormat::Json);
        assert_eq!("pretty".parse::<LogFormat>().unwrap(), LogFormat::Pretty);
        assert_eq!("text".parse::<LogFormat>().unwrap(), LogFormat::Pretty);
        assert!("xml".parse::<LogFormat>().is_err());
    }
}
