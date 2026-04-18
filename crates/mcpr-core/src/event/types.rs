//! Proxy event types carried through the event bus.
//!
//! [`ProxyEvent`] is the single event enum; sinks match on the variant to
//! decide what to do with each event.

use serde::Serialize;
use serde_json::Value;

/// All events flowing through the event bus.
///
/// Each variant represents a distinct lifecycle moment. Sinks match on
/// the variant to decide what to process.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProxyEvent {
    /// An MCP request completed (success or error).
    Request(Box<RequestEvent>),
    /// A new MCP session established via `initialize` handshake.
    SessionStart(SessionStartEvent),
    /// A session was closed (clean transport disconnect).
    SessionEnd(SessionEndEvent),
    /// Periodic health snapshot emitted by the health check loop.
    Heartbeat(HeartbeatEvent),
    /// A new `SchemaVersion` was created inside the proxy's `SchemaManager`.
    /// Emitted after pagination merge + change detection. Consumers
    /// (SQLite writer, cloud sink) persist or forward the version directly
    /// from the event — no secondary lookup required.
    SchemaVersionCreated(SchemaVersionCreatedEvent),
}

/// An MCP request that flowed through the proxy.
#[derive(Clone, Debug, Serialize)]
pub struct RequestEvent {
    /// Unique event ID (UUIDv4).
    pub id: String,
    /// Unix milliseconds (UTC).
    pub ts: i64,
    /// Proxy name (from config or derived from upstream URL).
    pub proxy: String,
    /// MCP session ID (from `mcp-session-id` header).
    pub session_id: Option<String>,

    /// HTTP method (POST, GET, DELETE).
    pub method: String,
    /// Request path.
    pub path: String,
    /// MCP JSON-RPC method (tools/call, resources/read, etc.).
    pub mcp_method: Option<String>,
    /// Tool name for `tools/call` requests.
    pub tool: Option<String>,

    /// HTTP response status code.
    pub status: u16,
    /// Wall-clock latency: proxy received request → sent response (μs).
    pub latency_us: u64,
    /// Time spent waiting for upstream response (μs).
    pub upstream_us: Option<u64>,
    /// Request payload size in bytes.
    pub request_size: Option<u64>,
    /// Response payload size in bytes.
    pub response_size: Option<u64>,

    /// MCP JSON-RPC error code (e.g., "-32600") if the response was an error.
    pub error_code: Option<String>,
    /// Error message (truncated to 512 chars).
    pub error_msg: Option<String>,

    /// Client name from session `clientInfo.name` (e.g., "claude-desktop").
    pub client_name: Option<String>,
    /// Client version from session `clientInfo.version` (e.g., "1.2.0").
    pub client_version: Option<String>,

    /// Classification note: "rewritten", "passthrough", "error", "sse", etc.
    pub note: String,
}

/// MCP session established via `initialize` handshake.
#[derive(Clone, Debug, Serialize)]
pub struct SessionStartEvent {
    pub session_id: String,
    pub proxy: String,
    pub ts: i64,
    /// Client name from `clientInfo.name` (e.g., "claude-desktop").
    pub client_name: Option<String>,
    /// Client version from `clientInfo.version` (e.g., "1.2.0").
    pub client_version: Option<String>,
    /// Normalized platform: "claude", "chatgpt", "vscode", "cursor", "unknown".
    pub client_platform: Option<String>,
}

/// Session closed (clean transport disconnect).
#[derive(Clone, Debug, Serialize)]
pub struct SessionEndEvent {
    pub session_id: String,
    pub ts: i64,
}

/// Periodic health snapshot.
#[derive(Clone, Debug, Serialize)]
pub struct HeartbeatEvent {
    pub ts: i64,
    pub proxy: String,
    pub mcp_status: String,
    pub tunnel_status: String,
    pub widgets_status: String,
    pub uptime_secs: u64,
    pub request_count: u64,
}

/// A new `SchemaVersion` was persisted for an upstream.
///
/// Carries the full merged payload so consumers (SQLite writer, cloud
/// sink) can persist or forward without a secondary lookup.
#[derive(Clone, Debug, Serialize)]
pub struct SchemaVersionCreatedEvent {
    /// Unix milliseconds (UTC).
    pub ts: i64,
    /// Proxy config name (upstream identity).
    pub upstream_id: String,
    /// Upstream MCP server URL (for legacy table rows keyed on url).
    pub upstream_url: String,
    /// MCP method that produced this version.
    pub method: String,
    /// Monotonic version number per (upstream, method).
    pub version: u32,
    /// Opaque `SchemaVersionId` (first 16 hex chars of `content_hash`).
    pub version_id: String,
    /// Full SHA-256 hex digest of the merged payload.
    pub content_hash: String,
    /// Merged `result` payload (post-pagination).
    pub payload: Value,
}
