use std::time::Instant;

use serde::Serialize;

/// A single request log entry capturing HTTP + MCP telemetry.
///
/// Constructed via the builder pattern:
/// ```ignore
/// LogEntry::new("POST", "/mcp", 200, "rewritten")
///     .mcp_method("tools/call")
///     .detail("get_weather")
///     .session_id("sid-123")
///     .upstream("http://localhost:9000/mcp")
///     .size(147)
///     .duration(start)
///     .upstream_duration(7)
/// ```
#[derive(Clone, Serialize)]
pub struct LogEntry {
    pub timestamp: String,
    /// ISO 8601 timestamp for machine-readable output (JSONL).
    pub timestamp_utc: String,
    pub method: String,
    pub path: String,
    pub mcp_method: Option<String>,
    pub session_id: Option<String>,
    pub status: u16,
    pub note: String,
    pub upstream_url: Option<String>,
    pub resp_size: Option<usize>,
    pub duration_ms: Option<u64>,
    /// Time spent waiting for upstream (network). Proxy overhead = duration_ms - upstream_ms.
    pub upstream_ms: Option<u64>,
    /// JSON-RPC error code from the response body (if the response is a JSON-RPC error).
    pub jsonrpc_error: Option<(i64, String)>,
    /// Extra detail: tool name for tools/call, resource URI for resources/read, etc.
    pub detail: Option<String>,
    /// Client identity from MCP initialize (e.g. "claude-desktop 1.2").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_name: Option<String>,
}

impl LogEntry {
    pub fn new(method: &str, path: &str, status: u16, note: &str) -> Self {
        let now = chrono::Utc::now();
        Self {
            timestamp: now
                .with_timezone(&chrono::Local)
                .format("%H:%M:%S")
                .to_string(),
            timestamp_utc: now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            method: method.to_string(),
            path: path.to_string(),
            mcp_method: None,
            session_id: None,
            status,
            note: note.to_string(),
            upstream_url: None,
            resp_size: None,
            duration_ms: None,
            upstream_ms: None,
            jsonrpc_error: None,
            detail: None,
            client_name: None,
        }
    }

    pub fn session_id(mut self, id: &str) -> Self {
        self.session_id = Some(id.to_string());
        self
    }

    pub fn maybe_session_id(mut self, id: Option<&str>) -> Self {
        self.session_id = id.map(String::from);
        self
    }

    pub fn mcp_method(mut self, m: &str) -> Self {
        self.mcp_method = Some(m.to_string());
        self
    }

    pub fn upstream(mut self, url: &str) -> Self {
        self.upstream_url = Some(url.to_string());
        self
    }

    pub fn size(mut self, bytes: usize) -> Self {
        self.resp_size = Some(bytes);
        self
    }

    pub fn duration(mut self, start: Instant) -> Self {
        self.duration_ms = Some(start.elapsed().as_millis() as u64);
        self
    }

    pub fn upstream_duration(mut self, ms: u64) -> Self {
        self.upstream_ms = Some(ms);
        self
    }

    pub fn jsonrpc_error(mut self, code: i64, message: &str) -> Self {
        self.jsonrpc_error = Some((code, message.to_string()));
        self
    }

    pub fn detail(mut self, d: &str) -> Self {
        self.detail = Some(d.to_string());
        self
    }

    pub fn maybe_detail(mut self, d: Option<&str>) -> Self {
        self.detail = d.map(String::from);
        self
    }

    pub fn client_name(mut self, name: &str) -> Self {
        self.client_name = Some(name.to_string());
        self
    }
}
