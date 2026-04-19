//! # mcpr-protocol
//!
//! MCP specification layer: JSON-RPC 2.0 parsing, MCP method classification,
//! and session lifecycle management.
//!
//! This crate is the foundation of the mcpr workspace. It contains everything
//! related to understanding the MCP protocol itself, with zero coupling to
//! HTTP frameworks or proxy logic.
//!
//! ## Responsibilities
//!
//! - **JSON-RPC 2.0 parsing** (`lib.rs`): Parse and classify JSON-RPC 2.0
//!   messages (requests, notifications, responses). Provides `ParsedBody` for
//!   batch-aware parsing and `McpMethod` for typed MCP method discrimination.
//!
//! - **MCP method constants**: Single source of truth for MCP method strings
//!   (`initialize`, `tools/call`, `resources/read`, etc.).
//!
//! - **Error handling**: JSON-RPC error codes, error response builders, and
//!   error extraction from response bodies.
//!
//! - **Session management** (`session` module): MCP session state machine
//!   (`Created -> Initialized -> Active -> Closed`), `SessionStore` trait for
//!   pluggable storage backends, and `MemorySessionStore` for in-memory use.
//!
//! - **Schema primitives** (`schema` module): Pagination detection/merging,
//!   schema diffing (detect added/removed/modified tools, resources,
//!   prompts), and schema method classification. Pure helpers, no state.
//!
//! - **Schema manager** (`schema_manager` module): Top-level per-upstream
//!   view of an MCP server. Owns versioned snapshots built from ingested
//!   discovery responses, exposes query APIs (list_tools, get_tool, ...),
//!   tracks stale flags, and defines the `SchemaScanner` trait for active
//!   discovery.
//!
//! ## Module Structure
//!
//! ```text
//! mcpr-protocol/src/
//! +-- lib.rs              # JSON-RPC 2.0 types, parsing, MCP method classification
//! +-- schema.rs           # Pagination merge + diff primitives
//! +-- schema_manager/     # SchemaManager, SchemaStore, SchemaScanner, SchemaVersion
//! +-- session.rs          # Session state, SessionStore trait, MemorySessionStore
//! ```
//!
//! ## Dependencies
//!
//! Minimal: `serde`, `serde_json`, `chrono`, `dashmap`. No HTTP framework deps.

pub mod schema;
pub mod schema_manager;
pub mod session;

use serde_json::Value;

// ── JSON-RPC 2.0 types ──

/// A parsed JSON-RPC 2.0 id (number or string).
#[derive(Debug, Clone, PartialEq)]
pub enum JsonRpcId {
    Number(i64),
    String(String),
}

impl std::fmt::Display for JsonRpcId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Number(n) => write!(f, "{n}"),
            Self::String(s) => write!(f, "{s}"),
        }
    }
}

/// A classified JSON-RPC 2.0 message.
#[derive(Debug)]
pub enum JsonRpcMessage {
    /// Has `method` + `id` → expects a response.
    Request(JsonRpcRequest),
    /// Has `method`, no `id` → fire-and-forget.
    Notification(JsonRpcNotification),
    /// Has `id` + `result`/`error` → reply to a prior request.
    Response(JsonRpcResponse),
}

#[derive(Debug)]
pub struct JsonRpcRequest {
    pub id: JsonRpcId,
    pub method: String,
    pub params: Option<Value>,
}

#[derive(Debug)]
pub struct JsonRpcNotification {
    pub method: String,
    pub params: Option<Value>,
}

#[derive(Debug)]
pub struct JsonRpcResponse {
    pub id: JsonRpcId,
    pub result: Option<Value>,
    pub error: Option<JsonRpcError>,
}

#[derive(Debug)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    pub data: Option<Value>,
}

// ── JSON-RPC 2.0 error codes (spec: https://www.jsonrpc.org/specification#error_object) ──

pub mod error_code {
    /// Invalid JSON was received.
    pub const PARSE_ERROR: i64 = -32700;
    /// The JSON sent is not a valid Request object.
    pub const INVALID_REQUEST: i64 = -32600;
    /// The method does not exist / is not available.
    pub const METHOD_NOT_FOUND: i64 = -32601;
    /// Invalid method parameter(s).
    pub const INVALID_PARAMS: i64 = -32602;
    /// Internal JSON-RPC error.
    pub const INTERNAL_ERROR: i64 = -32603;

    /// Short label for known error codes.
    pub fn label(code: i64) -> &'static str {
        match code {
            PARSE_ERROR => "Parse error",
            INVALID_REQUEST => "Invalid request",
            METHOD_NOT_FOUND => "Method not found",
            INVALID_PARAMS => "Invalid params",
            INTERNAL_ERROR => "Internal error",
            -32099..=-32000 => "Server error",
            _ => "Unknown error",
        }
    }
}

// ── JSON-RPC error response builder ──

/// Build a JSON-RPC 2.0 error response as bytes.
/// `id` can be a request id or `Value::Null` if the request id couldn't be parsed.
pub fn error_response(id: &Value, code: i64, message: &str) -> Vec<u8> {
    let resp = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": code,
            "message": message,
        }
    });
    serde_json::to_vec(&resp).unwrap_or_default()
}

/// Extract the JSON-RPC error code from a response body (if it's an error response).
pub fn extract_error_code(body: &Value) -> Option<(i64, &str)> {
    let err = body.get("error")?;
    let code = err.get("code")?.as_i64()?;
    let message = err.get("message")?.as_str()?;
    Some((code, message))
}

// ── MCP method classification ──

/// Known MCP methods — lets the proxy know exactly what function is being called.
#[derive(Debug, Clone, PartialEq)]
pub enum McpMethod {
    Initialize,
    Initialized,
    Ping,
    ToolsList,
    ToolsCall,
    ResourcesList,
    ResourcesRead,
    ResourcesTemplatesList,
    ResourcesSubscribe,
    ResourcesUnsubscribe,
    PromptsList,
    PromptsGet,
    LoggingSetLevel,
    CompletionComplete,
    NotificationsToolsListChanged,
    NotificationsCancelled,
    NotificationsProgress,
    /// Any `notifications/*` we don't have a specific variant for.
    Notification(String),
    /// Anything else.
    Unknown(String),
}

impl McpMethod {
    /// `true` for methods whose responses may need body rewriting (CSP
    /// injection in `meta`, widget overlay substitution in `contents`).
    /// Callers use this to pick buffer-vs-stream strategy pre-forward.
    ///
    /// Only the five methods that carry `_meta` / widget payloads return
    /// `true`. Everything else — initialize, ping, notifications, prompts,
    /// completion, logging — can safely stream.
    pub fn needs_response_buffering(&self) -> bool {
        matches!(
            self,
            McpMethod::ToolsList
                | McpMethod::ToolsCall
                | McpMethod::ResourcesList
                | McpMethod::ResourcesTemplatesList
                | McpMethod::ResourcesRead
        )
    }
}

// MCP method name constants — single source of truth for string matching.
pub const INITIALIZE: &str = "initialize";
pub const INITIALIZED: &str = "notifications/initialized";
pub const PING: &str = "ping";
pub const TOOLS_LIST: &str = "tools/list";
pub const TOOLS_CALL: &str = "tools/call";
pub const RESOURCES_LIST: &str = "resources/list";
pub const RESOURCES_READ: &str = "resources/read";
pub const RESOURCES_SUBSCRIBE: &str = "resources/subscribe";
pub const RESOURCES_UNSUBSCRIBE: &str = "resources/unsubscribe";
pub const PROMPTS_LIST: &str = "prompts/list";
pub const PROMPTS_GET: &str = "prompts/get";
pub const LOGGING_SET_LEVEL: &str = "logging/setLevel";
pub const COMPLETION_COMPLETE: &str = "completion/complete";
pub const RESOURCES_TEMPLATES_LIST: &str = "resources/templates/list";
pub const NOTIFICATIONS_TOOLS_LIST_CHANGED: &str = "notifications/tools/list_changed";
pub const NOTIFICATIONS_CANCELLED: &str = "notifications/cancelled";
pub const NOTIFICATIONS_PROGRESS: &str = "notifications/progress";

impl McpMethod {
    pub fn parse(method: &str) -> Self {
        match method {
            INITIALIZE => Self::Initialize,
            INITIALIZED => Self::Initialized,
            PING => Self::Ping,
            TOOLS_LIST => Self::ToolsList,
            TOOLS_CALL => Self::ToolsCall,
            RESOURCES_LIST => Self::ResourcesList,
            RESOURCES_READ => Self::ResourcesRead,
            RESOURCES_TEMPLATES_LIST => Self::ResourcesTemplatesList,
            RESOURCES_SUBSCRIBE => Self::ResourcesSubscribe,
            RESOURCES_UNSUBSCRIBE => Self::ResourcesUnsubscribe,
            PROMPTS_LIST => Self::PromptsList,
            PROMPTS_GET => Self::PromptsGet,
            LOGGING_SET_LEVEL => Self::LoggingSetLevel,
            COMPLETION_COMPLETE => Self::CompletionComplete,
            NOTIFICATIONS_TOOLS_LIST_CHANGED => Self::NotificationsToolsListChanged,
            NOTIFICATIONS_CANCELLED => Self::NotificationsCancelled,
            NOTIFICATIONS_PROGRESS => Self::NotificationsProgress,
            m if m.starts_with("notifications/") => Self::Notification(m.to_string()),
            m => Self::Unknown(m.to_string()),
        }
    }

    /// Short label for logging (e.g. "tools/call", "initialize").
    pub fn as_str(&self) -> &str {
        match self {
            Self::Initialize => INITIALIZE,
            Self::Initialized => INITIALIZED,
            Self::Ping => PING,
            Self::ToolsList => TOOLS_LIST,
            Self::ToolsCall => TOOLS_CALL,
            Self::ResourcesList => RESOURCES_LIST,
            Self::ResourcesRead => RESOURCES_READ,
            Self::ResourcesTemplatesList => RESOURCES_TEMPLATES_LIST,
            Self::ResourcesSubscribe => RESOURCES_SUBSCRIBE,
            Self::ResourcesUnsubscribe => RESOURCES_UNSUBSCRIBE,
            Self::PromptsList => PROMPTS_LIST,
            Self::PromptsGet => PROMPTS_GET,
            Self::LoggingSetLevel => LOGGING_SET_LEVEL,
            Self::CompletionComplete => COMPLETION_COMPLETE,
            Self::NotificationsToolsListChanged => NOTIFICATIONS_TOOLS_LIST_CHANGED,
            Self::NotificationsCancelled => NOTIFICATIONS_CANCELLED,
            Self::NotificationsProgress => NOTIFICATIONS_PROGRESS,
            Self::Notification(m) => m.as_str(),
            Self::Unknown(m) => m.as_str(),
        }
    }
}

// ── Parsing ──

/// Parse a JSON-RPC id value.
fn parse_id(value: &Value) -> Option<JsonRpcId> {
    match value {
        Value::Number(n) => n.as_i64().map(JsonRpcId::Number),
        Value::String(s) => Some(JsonRpcId::String(s.clone())),
        _ => None,
    }
}

/// Parse a JSON-RPC error object.
fn parse_error(value: &Value) -> Option<JsonRpcError> {
    let obj = value.as_object()?;
    Some(JsonRpcError {
        code: obj.get("code")?.as_i64()?,
        message: obj.get("message")?.as_str()?.to_string(),
        data: obj.get("data").cloned(),
    })
}

/// Parse a single JSON value as a JSON-RPC 2.0 message.
/// Returns `None` if it doesn't have `"jsonrpc": "2.0"`.
pub fn parse_message(value: &Value) -> Option<JsonRpcMessage> {
    let obj = value.as_object()?;

    // Must be JSON-RPC 2.0
    if obj.get("jsonrpc")?.as_str()? != "2.0" {
        return None;
    }

    let id = obj.get("id").and_then(parse_id);
    let method = obj.get("method").and_then(|m| m.as_str()).map(String::from);
    let params = obj.get("params").cloned();

    match (method, id) {
        // Has method + id → Request
        (Some(method), Some(id)) => Some(JsonRpcMessage::Request(JsonRpcRequest {
            id,
            method,
            params,
        })),
        // Has method, no id → Notification
        (Some(method), None) => Some(JsonRpcMessage::Notification(JsonRpcNotification {
            method,
            params,
        })),
        // No method, has id → Response
        (None, Some(id)) => {
            let result = obj.get("result").cloned();
            let error = obj.get("error").and_then(parse_error);
            Some(JsonRpcMessage::Response(JsonRpcResponse {
                id,
                result,
                error,
            }))
        }
        // No method, no id → invalid
        (None, None) => None,
    }
}

/// Result of parsing a POST body as JSON-RPC 2.0.
#[derive(Debug)]
pub struct ParsedBody {
    pub messages: Vec<JsonRpcMessage>,
    pub is_batch: bool,
}

impl ParsedBody {
    /// Get the method string from the first request or notification.
    /// Falls back to "unknown" if the batch contains only responses.
    pub fn method_str(&self) -> &str {
        self.messages
            .iter()
            .find_map(|m| match m {
                JsonRpcMessage::Request(r) => Some(r.method.as_str()),
                JsonRpcMessage::Notification(n) => Some(n.method.as_str()),
                _ => None,
            })
            .unwrap_or("unknown")
    }

    /// Get the MCP method classification from the first request/notification.
    pub fn mcp_method(&self) -> McpMethod {
        McpMethod::parse(self.method_str())
    }

    /// Get the id of the first request (if any).
    pub fn first_request_id(&self) -> Option<&JsonRpcId> {
        self.messages.iter().find_map(|m| match m {
            JsonRpcMessage::Request(r) => Some(&r.id),
            _ => None,
        })
    }

    /// True if every message is a notification (no id, no response expected).
    pub fn is_notification_only(&self) -> bool {
        self.messages
            .iter()
            .all(|m| matches!(m, JsonRpcMessage::Notification(_)))
    }

    /// Extract a short detail string for logging:
    /// - tools/call → tool name (params.name)
    /// - resources/read → resource URI (params.uri)
    /// - prompts/get → prompt name (params.name)
    pub fn detail(&self) -> Option<String> {
        let params = self.first_params()?;
        let method = self.mcp_method();
        match method {
            McpMethod::ToolsCall => params.get("name")?.as_str().map(String::from),
            McpMethod::ResourcesRead => params.get("uri")?.as_str().map(String::from),
            McpMethod::PromptsGet => params.get("name")?.as_str().map(String::from),
            McpMethod::NotificationsCancelled => {
                // requestId can be string or number
                params.get("requestId").map(|v| match v {
                    Value::String(s) => s.clone(),
                    Value::Number(n) => n.to_string(),
                    _ => v.to_string(),
                })
            }
            McpMethod::NotificationsProgress => {
                // progressToken can be string or number
                params.get("progressToken").map(|v| match v {
                    Value::String(s) => s.clone(),
                    Value::Number(n) => n.to_string(),
                    _ => v.to_string(),
                })
            }
            _ => None,
        }
    }

    /// Get the raw params from the first request/notification.
    pub fn first_params(&self) -> Option<&Value> {
        self.messages.iter().find_map(|m| match m {
            JsonRpcMessage::Request(r) => r.params.as_ref(),
            JsonRpcMessage::Notification(n) => n.params.as_ref(),
            _ => None,
        })
    }
}

/// Parse a POST body as JSON-RPC 2.0 — single message or batch.
/// Returns `None` if the body is not valid JSON-RPC 2.0.
pub fn parse_body(body: &[u8]) -> Option<ParsedBody> {
    let value: Value = serde_json::from_slice(body).ok()?;

    if let Some(arr) = value.as_array() {
        // Batch: array of JSON-RPC messages
        let messages: Vec<_> = arr.iter().filter_map(parse_message).collect();
        if messages.is_empty() {
            return None;
        }
        Some(ParsedBody {
            messages,
            is_batch: true,
        })
    } else {
        // Single message
        let msg = parse_message(&value)?;
        Some(ParsedBody {
            messages: vec![msg],
            is_batch: false,
        })
    }
}

// ── Tests ──

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── parse_message ──

    #[test]
    fn parse_message__request() {
        let val = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "get_weather"}});
        let msg = parse_message(&val).unwrap();
        match msg {
            JsonRpcMessage::Request(r) => {
                assert_eq!(r.id, JsonRpcId::Number(1));
                assert_eq!(r.method, "tools/call");
                assert!(r.params.is_some());
            }
            _ => panic!("expected Request"),
        }
    }

    #[test]
    fn parse_message__request_string_id() {
        let val = json!({"jsonrpc": "2.0", "id": "abc-123", "method": "initialize"});
        let msg = parse_message(&val).unwrap();
        match msg {
            JsonRpcMessage::Request(r) => {
                assert_eq!(r.id, JsonRpcId::String("abc-123".into()));
                assert_eq!(r.method, "initialize");
            }
            _ => panic!("expected Request"),
        }
    }

    #[test]
    fn parse_message__notification() {
        let val = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
        let msg = parse_message(&val).unwrap();
        match msg {
            JsonRpcMessage::Notification(n) => {
                assert_eq!(n.method, "notifications/initialized");
                assert!(n.params.is_none());
            }
            _ => panic!("expected Notification"),
        }
    }

    #[test]
    fn parse_message__response_result() {
        let val = json!({"jsonrpc": "2.0", "id": 1, "result": {"tools": []}});
        let msg = parse_message(&val).unwrap();
        match msg {
            JsonRpcMessage::Response(r) => {
                assert_eq!(r.id, JsonRpcId::Number(1));
                assert!(r.result.is_some());
                assert!(r.error.is_none());
            }
            _ => panic!("expected Response"),
        }
    }

    #[test]
    fn parse_message__response_error() {
        let val = json!({"jsonrpc": "2.0", "id": 1, "error": {"code": -32601, "message": "Method not found"}});
        let msg = parse_message(&val).unwrap();
        match msg {
            JsonRpcMessage::Response(r) => {
                assert_eq!(r.id, JsonRpcId::Number(1));
                assert!(r.result.is_none());
                let err = r.error.unwrap();
                assert_eq!(err.code, -32601);
                assert_eq!(err.message, "Method not found");
            }
            _ => panic!("expected Response"),
        }
    }

    #[test]
    fn parse_message__rejects_wrong_version() {
        let val = json!({"jsonrpc": "1.0", "id": 1, "method": "test"});
        assert!(parse_message(&val).is_none());
    }

    #[test]
    fn parse_message__rejects_missing_jsonrpc() {
        let val = json!({"id": 1, "method": "test"});
        assert!(parse_message(&val).is_none());
    }

    #[test]
    fn parse_message__rejects_no_method_no_id() {
        let val = json!({"jsonrpc": "2.0"});
        assert!(parse_message(&val).is_none());
    }

    #[test]
    fn parse_message__rejects_non_object() {
        let val = json!("hello");
        assert!(parse_message(&val).is_none());
    }

    #[test]
    fn parse_message__rejects_oauth_register() {
        let val = json!({
            "client_name": "My App",
            "redirect_uris": ["https://example.com/callback"],
            "grant_types": ["authorization_code"]
        });
        assert!(parse_message(&val).is_none());
    }

    // ── parse_body ──

    #[test]
    fn parse_body__single_request() {
        let body = br#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
        let parsed = parse_body(body).unwrap();
        assert!(!parsed.is_batch);
        assert_eq!(parsed.messages.len(), 1);
        assert_eq!(parsed.method_str(), "tools/list");
        assert_eq!(parsed.mcp_method(), McpMethod::ToolsList);
    }

    #[test]
    fn parse_body__batch_requests() {
        let body = br#"[
            {"jsonrpc":"2.0","id":1,"method":"tools/list"},
            {"jsonrpc":"2.0","id":2,"method":"resources/list"}
        ]"#;
        let parsed = parse_body(body).unwrap();
        assert!(parsed.is_batch);
        assert_eq!(parsed.messages.len(), 2);
        assert_eq!(parsed.method_str(), "tools/list");
    }

    #[test]
    fn parse_body__notification_only() {
        let body = br#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
        let parsed = parse_body(body).unwrap();
        assert!(parsed.is_notification_only());
        assert_eq!(parsed.mcp_method(), McpMethod::Initialized);
    }

    #[test]
    fn parse_body__mixed_batch() {
        let body = br#"[
            {"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":1}},
            {"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"get_weather"}}
        ]"#;
        let parsed = parse_body(body).unwrap();
        assert!(parsed.is_batch);
        assert!(!parsed.is_notification_only());
        assert_eq!(parsed.first_request_id(), Some(&JsonRpcId::Number(2)));
    }

    #[test]
    fn parse_body__rejects_empty_batch() {
        let body = b"[]";
        assert!(parse_body(body).is_none());
    }

    #[test]
    fn parse_body__rejects_invalid_json() {
        assert!(parse_body(b"not json").is_none());
    }

    #[test]
    fn parse_body__rejects_non_jsonrpc() {
        let body = br#"{"grant_type":"client_credentials","client_id":"abc"}"#;
        assert!(parse_body(body).is_none());
    }

    #[test]
    fn parse_body__rejects_batch_of_non_jsonrpc() {
        let body = br#"[{"foo":"bar"},{"baz":1}]"#;
        assert!(parse_body(body).is_none());
    }

    // ── McpMethod ──

    #[test]
    fn mcp_method__known_methods() {
        assert_eq!(McpMethod::parse("initialize"), McpMethod::Initialize);
        assert_eq!(McpMethod::parse("tools/call"), McpMethod::ToolsCall);
        assert_eq!(McpMethod::parse("tools/list"), McpMethod::ToolsList);
        assert_eq!(McpMethod::parse("resources/read"), McpMethod::ResourcesRead);
        assert_eq!(McpMethod::parse("resources/list"), McpMethod::ResourcesList);
        assert_eq!(
            McpMethod::parse("resources/templates/list"),
            McpMethod::ResourcesTemplatesList
        );
        assert_eq!(McpMethod::parse("prompts/list"), McpMethod::PromptsList);
        assert_eq!(McpMethod::parse("prompts/get"), McpMethod::PromptsGet);
        assert_eq!(McpMethod::parse("ping"), McpMethod::Ping);
        assert_eq!(
            McpMethod::parse("logging/setLevel"),
            McpMethod::LoggingSetLevel
        );
        assert_eq!(
            McpMethod::parse("completion/complete"),
            McpMethod::CompletionComplete
        );
        assert_eq!(
            McpMethod::parse("notifications/tools/list_changed"),
            McpMethod::NotificationsToolsListChanged
        );
        assert_eq!(
            McpMethod::parse("notifications/cancelled"),
            McpMethod::NotificationsCancelled
        );
        assert_eq!(
            McpMethod::parse("notifications/progress"),
            McpMethod::NotificationsProgress
        );
    }

    #[test]
    fn mcp_method__notifications() {
        assert_eq!(
            McpMethod::parse("notifications/initialized"),
            McpMethod::Initialized
        );
        // Known notification variants are parsed to specific enum values
        assert_eq!(
            McpMethod::parse("notifications/cancelled"),
            McpMethod::NotificationsCancelled
        );
        assert_eq!(
            McpMethod::parse("notifications/progress"),
            McpMethod::NotificationsProgress
        );
        assert_eq!(
            McpMethod::parse("notifications/tools/list_changed"),
            McpMethod::NotificationsToolsListChanged
        );
        // Unknown notifications still fall through to the generic variant
        assert_eq!(
            McpMethod::parse("notifications/resources/updated"),
            McpMethod::Notification("notifications/resources/updated".into())
        );
    }

    #[test]
    fn mcp_method__unknown() {
        assert_eq!(
            McpMethod::parse("custom/method"),
            McpMethod::Unknown("custom/method".into())
        );
    }

    #[test]
    fn mcp_method__as_str_roundtrip() {
        let methods = [
            "initialize",
            "notifications/initialized",
            "ping",
            "tools/list",
            "tools/call",
            "resources/list",
            "resources/read",
            "resources/templates/list",
            "prompts/list",
            "prompts/get",
            "logging/setLevel",
            "completion/complete",
            "notifications/tools/list_changed",
            "notifications/cancelled",
            "notifications/progress",
        ];
        for m in methods {
            assert_eq!(McpMethod::parse(m).as_str(), m);
        }
    }

    // ── JsonRpcId display ──

    #[test]
    fn jsonrpc_id__display() {
        assert_eq!(JsonRpcId::Number(42).to_string(), "42");
        assert_eq!(JsonRpcId::String("abc".into()).to_string(), "abc");
    }

    // ── ParsedBody helpers ──

    #[test]
    fn parsed_body__first_params_from_request() {
        let body = br#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"echo"}}"#;
        let parsed = parse_body(body).unwrap();
        let params = parsed.first_params().unwrap();
        assert_eq!(params["name"], "echo");
    }

    #[test]
    fn parsed_body__first_params_none_for_response() {
        let body = br#"{"jsonrpc":"2.0","id":1,"result":{}}"#;
        let parsed = parse_body(body).unwrap();
        assert!(parsed.first_params().is_none());
    }

    #[test]
    fn parsed_body__method_str_defaults_to_unknown() {
        let body = br#"{"jsonrpc":"2.0","id":1,"result":{"tools":[]}}"#;
        let parsed = parse_body(body).unwrap();
        assert_eq!(parsed.method_str(), "unknown");
    }

    // ── ParsedBody::detail ──

    #[test]
    fn parsed_body__detail_tools_call() {
        let body =
            br#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"get_weather"}}"#;
        let parsed = parse_body(body).unwrap();
        assert_eq!(parsed.detail().as_deref(), Some("get_weather"));
    }

    #[test]
    fn parsed_body__detail_resources_read() {
        let body = br#"{"jsonrpc":"2.0","id":1,"method":"resources/read","params":{"uri":"ui://widget/clock.html"}}"#;
        let parsed = parse_body(body).unwrap();
        assert_eq!(parsed.detail().as_deref(), Some("ui://widget/clock.html"));
    }

    #[test]
    fn parsed_body__detail_none_for_tools_list() {
        let body = br#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
        let parsed = parse_body(body).unwrap();
        assert!(parsed.detail().is_none());
    }

    #[test]
    fn parsed_body__detail_notifications_cancelled() {
        let body = br#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":"req-42","reason":"timeout"}}"#;
        let parsed = parse_body(body).unwrap();
        assert_eq!(parsed.detail().as_deref(), Some("req-42"));
    }

    #[test]
    fn parsed_body__detail_cancelled_numeric_id() {
        let body =
            br#"{"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":7}}"#;
        let parsed = parse_body(body).unwrap();
        assert_eq!(parsed.detail().as_deref(), Some("7"));
    }

    #[test]
    fn parsed_body__detail_notifications_progress() {
        let body = br#"{"jsonrpc":"2.0","method":"notifications/progress","params":{"progressToken":"tok-1","progress":50,"total":100}}"#;
        let parsed = parse_body(body).unwrap();
        assert_eq!(parsed.detail().as_deref(), Some("tok-1"));
    }

    #[test]
    fn parsed_body__detail_progress_numeric_token() {
        let body = br#"{"jsonrpc":"2.0","method":"notifications/progress","params":{"progressToken":99,"progress":10}}"#;
        let parsed = parse_body(body).unwrap();
        assert_eq!(parsed.detail().as_deref(), Some("99"));
    }

    // ── error_code ──

    #[test]
    fn error_code__labels() {
        assert_eq!(error_code::label(error_code::PARSE_ERROR), "Parse error");
        assert_eq!(
            error_code::label(error_code::METHOD_NOT_FOUND),
            "Method not found"
        );
        assert_eq!(
            error_code::label(error_code::INVALID_PARAMS),
            "Invalid params"
        );
        assert_eq!(
            error_code::label(error_code::INTERNAL_ERROR),
            "Internal error"
        );
        assert_eq!(error_code::label(-32000), "Server error");
        assert_eq!(error_code::label(-32099), "Server error");
        assert_eq!(error_code::label(42), "Unknown error");
    }

    // ── error_response ──

    #[test]
    fn error_response__numeric_id() {
        let body = error_response(&json!(1), error_code::METHOD_NOT_FOUND, "Method not found");
        let parsed: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(parsed["jsonrpc"], "2.0");
        assert_eq!(parsed["id"], 1);
        assert_eq!(parsed["error"]["code"], -32601);
        assert_eq!(parsed["error"]["message"], "Method not found");
    }

    #[test]
    fn error_response__null_id() {
        let body = error_response(&Value::Null, error_code::PARSE_ERROR, "Parse error");
        let parsed: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(parsed["id"], Value::Null);
        assert_eq!(parsed["error"]["code"], -32700);
    }

    // ── extract_error_code ──

    #[test]
    fn extract_error_code__from_error_response() {
        let val = json!({"jsonrpc": "2.0", "id": 1, "error": {"code": -32601, "message": "Method not found"}});
        let (code, msg) = extract_error_code(&val).unwrap();
        assert_eq!(code, -32601);
        assert_eq!(msg, "Method not found");
    }

    #[test]
    fn extract_error_code__none_for_success() {
        let val = json!({"jsonrpc": "2.0", "id": 1, "result": {"tools": []}});
        assert!(extract_error_code(&val).is_none());
    }
}
