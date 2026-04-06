use chrono::{DateTime, Utc};
use serde::Serialize;

/// Structured event emitted by the mcpr proxy.
#[derive(Debug, Clone, Serialize)]
pub struct McprEvent {
    /// ISO 8601 timestamp
    pub ts: DateTime<Utc>,
    /// Event type discriminator
    #[serde(rename = "type")]
    pub event_type: EventType,
    /// MCP method (e.g. "tools/call", "tools/list")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    /// Tool name for tool_call events
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    /// MCP session ID
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// Authenticated user (when OAuth is enabled)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// Request latency in milliseconds
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u64>,
    /// Request outcome
    pub status: EventStatus,
    /// Which upstream handled this request
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upstream: Option<String>,
    /// Whether CSP headers were injected
    #[serde(skip_serializing_if = "Option::is_none")]
    pub csp_applied: Option<bool>,
    /// Server slug for cloud routing
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    /// Client name from MCP initialize clientInfo
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_name: Option<String>,
    /// Client version from MCP initialize clientInfo
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_version: Option<String>,
    /// Request body size in bytes
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_size: Option<u64>,
    /// Response body size in bytes
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_size: Option<u64>,
    /// Error message when status = error
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_detail: Option<String>,
    /// Extensible metadata
    #[serde(skip_serializing_if = "Option::is_none")]
    pub meta: Option<serde_json::Value>,
    /// Schema version
    #[serde(default = "default_version")]
    pub v: u8,
}

/// Default schema version for serde deserialization.
#[allow(dead_code)]
fn default_version() -> u8 {
    1
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EventType {
    ToolCall,
    ToolList,
    SessionStart,
    SessionEnd,
    WidgetServe,
    CspViolation,
    AuthEvent,
    AclDeny,
    /// Catch-all for passthrough requests (OAuth, well-known, etc.)
    Request,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EventStatus {
    Ok,
    Error,
    Denied,
}

impl McprEvent {
    /// Create a new event with timestamp set to now.
    pub fn new(event_type: EventType) -> Self {
        Self {
            ts: Utc::now(),
            event_type,
            method: None,
            tool: None,
            session: None,
            user: None,
            latency_ms: None,
            status: EventStatus::Ok,
            upstream: None,
            csp_applied: None,
            server: None,
            client_name: None,
            client_version: None,
            request_size: None,
            response_size: None,
            error_detail: None,
            meta: None,
            v: 1,
        }
    }

    pub fn method(mut self, m: impl Into<String>) -> Self {
        self.method = Some(m.into());
        self
    }

    pub fn tool(mut self, t: impl Into<String>) -> Self {
        self.tool = Some(t.into());
        self
    }

    pub fn session(mut self, s: impl Into<String>) -> Self {
        self.session = Some(s.into());
        self
    }

    pub fn latency(mut self, ms: u64) -> Self {
        self.latency_ms = Some(ms);
        self
    }

    pub fn status(mut self, s: EventStatus) -> Self {
        self.status = s;
        self
    }

    pub fn upstream(mut self, u: impl Into<String>) -> Self {
        self.upstream = Some(u.into());
        self
    }

    pub fn csp(mut self, applied: bool) -> Self {
        self.csp_applied = Some(applied);
        self
    }

    pub fn server(mut self, s: impl Into<String>) -> Self {
        self.server = Some(s.into());
        self
    }

    pub fn error_detail(mut self, d: impl Into<String>) -> Self {
        self.error_detail = Some(d.into());
        self
    }

    pub fn request_size(mut self, n: u64) -> Self {
        self.request_size = Some(n);
        self
    }

    pub fn response_size(mut self, n: u64) -> Self {
        self.response_size = Some(n);
        self
    }
}
