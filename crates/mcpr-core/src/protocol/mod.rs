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
//! - **JSON-RPC 2.0 types** (this module): `JsonRpcId`, `JsonRpcError`,
//!   `error_code`, `error_response` / `extract_error_code`, and `McpMethod`
//!   for typed method discrimination. The shallow JSON-RPC envelope used
//!   by the pipeline lives in `proxy::pipeline::envelope` — this module
//!   owns the spec-level types and helpers, not the parse path.
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

// ── Tests ──

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;
    use serde_json::json;

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
