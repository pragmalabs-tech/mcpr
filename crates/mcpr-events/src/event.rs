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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_event_has_correct_defaults() {
        let event = McprEvent::new(EventType::ToolCall);
        assert!(event.method.is_none());
        assert!(event.tool.is_none());
        assert!(event.session.is_none());
        assert!(event.user.is_none());
        assert!(event.latency_ms.is_none());
        assert!(event.upstream.is_none());
        assert!(event.csp_applied.is_none());
        assert!(event.server.is_none());
        assert!(event.client_name.is_none());
        assert!(event.client_version.is_none());
        assert!(event.request_size.is_none());
        assert!(event.response_size.is_none());
        assert!(event.error_detail.is_none());
        assert!(event.meta.is_none());
        assert_eq!(event.v, 1);
        assert!(matches!(event.status, EventStatus::Ok));
    }

    #[test]
    fn builder_methods_set_fields() {
        let event = McprEvent::new(EventType::ToolCall)
            .method("tools/call")
            .tool("get_weather")
            .session("sess-123")
            .latency(42)
            .status(EventStatus::Error)
            .upstream("http://localhost:9000")
            .csp(true)
            .server("my-proxy")
            .error_detail("timeout")
            .request_size(1024)
            .response_size(2048);

        assert_eq!(event.method.as_deref(), Some("tools/call"));
        assert_eq!(event.tool.as_deref(), Some("get_weather"));
        assert_eq!(event.session.as_deref(), Some("sess-123"));
        assert_eq!(event.latency_ms, Some(42));
        assert!(matches!(event.status, EventStatus::Error));
        assert_eq!(event.upstream.as_deref(), Some("http://localhost:9000"));
        assert_eq!(event.csp_applied, Some(true));
        assert_eq!(event.server.as_deref(), Some("my-proxy"));
        assert_eq!(event.error_detail.as_deref(), Some("timeout"));
        assert_eq!(event.request_size, Some(1024));
        assert_eq!(event.response_size, Some(2048));
    }

    #[test]
    fn serialization_omits_none_fields() {
        let event = McprEvent::new(EventType::ToolCall);
        let json = serde_json::to_value(&event).unwrap();
        let obj = json.as_object().unwrap();

        // Required fields are always present.
        assert!(obj.contains_key("ts"));
        assert!(obj.contains_key("type"));
        assert!(obj.contains_key("status"));
        assert!(obj.contains_key("v"));

        // Optional None fields should be omitted.
        assert!(!obj.contains_key("method"));
        assert!(!obj.contains_key("tool"));
        assert!(!obj.contains_key("session"));
        assert!(!obj.contains_key("user"));
        assert!(!obj.contains_key("latency_ms"));
        assert!(!obj.contains_key("upstream"));
        assert!(!obj.contains_key("csp_applied"));
        assert!(!obj.contains_key("server"));
        assert!(!obj.contains_key("client_name"));
        assert!(!obj.contains_key("client_version"));
        assert!(!obj.contains_key("request_size"));
        assert!(!obj.contains_key("response_size"));
        assert!(!obj.contains_key("error_detail"));
        assert!(!obj.contains_key("meta"));
    }

    #[test]
    fn serialization_includes_set_fields() {
        let event = McprEvent::new(EventType::ToolCall)
            .method("tools/call")
            .tool("read_file")
            .server("proxy-1")
            .request_size(512)
            .response_size(1024)
            .error_detail("not found");

        let json = serde_json::to_value(&event).unwrap();
        let obj = json.as_object().unwrap();

        assert_eq!(obj["method"], "tools/call");
        assert_eq!(obj["tool"], "read_file");
        assert_eq!(obj["server"], "proxy-1");
        assert_eq!(obj["request_size"], 512);
        assert_eq!(obj["response_size"], 1024);
        assert_eq!(obj["error_detail"], "not found");
    }

    #[test]
    fn event_type_serializes_as_snake_case() {
        let cases = vec![
            (EventType::ToolCall, "tool_call"),
            (EventType::ToolList, "tool_list"),
            (EventType::SessionStart, "session_start"),
            (EventType::SessionEnd, "session_end"),
            (EventType::WidgetServe, "widget_serve"),
            (EventType::CspViolation, "csp_violation"),
            (EventType::AuthEvent, "auth_event"),
            (EventType::AclDeny, "acl_deny"),
            (EventType::Request, "request"),
        ];

        for (event_type, expected) in cases {
            let event = McprEvent::new(event_type);
            let json = serde_json::to_value(&event).unwrap();
            assert_eq!(json["type"], expected);
        }
    }

    #[test]
    fn event_status_serializes_as_snake_case() {
        let event_ok = McprEvent::new(EventType::ToolCall).status(EventStatus::Ok);
        let event_err = McprEvent::new(EventType::ToolCall).status(EventStatus::Error);
        let event_denied = McprEvent::new(EventType::ToolCall).status(EventStatus::Denied);

        assert_eq!(serde_json::to_value(&event_ok).unwrap()["status"], "ok");
        assert_eq!(serde_json::to_value(&event_err).unwrap()["status"], "error");
        assert_eq!(
            serde_json::to_value(&event_denied).unwrap()["status"],
            "denied"
        );
    }

    #[test]
    fn schema_version_defaults_to_1() {
        let event = McprEvent::new(EventType::Request);
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["v"], 1);
    }

    #[test]
    fn batch_serialization_produces_valid_json_array() {
        let events = vec![
            McprEvent::new(EventType::ToolCall).method("tools/call"),
            McprEvent::new(EventType::SessionStart).session("s1"),
        ];

        let json_bytes = serde_json::to_vec(&events).unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&json_bytes).unwrap();
        let arr = parsed.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["type"], "tool_call");
        assert_eq!(arr[1]["type"], "session_start");
    }
}
