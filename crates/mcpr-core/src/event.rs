//! Proxy event types and the event sink trait.
//!
//! [`ProxyEvent`] is the single event enum flowing through the event bus.
//! [`EventSink`] is the trait sinks implement to consume events.
//!
//! Both live in `mcpr-core` so any crate can:
//! - Emit events (proxy engine)
//! - Consume events (sinks: stderr, sqlite, cloud, prometheus, etc.)

use mcpr_protocol::schema::PageStatus;
use serde::Serialize;

// ── Event types ────────────────────────────────────────────────────────

/// All events flowing through the event bus.
///
/// Each variant represents a distinct lifecycle moment. Sinks match on
/// the variant to decide what to process.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProxyEvent {
    /// An MCP request completed (success or error).
    Request(RequestEvent),
    /// A new MCP session established via `initialize` handshake.
    SessionStart(SessionStartEvent),
    /// A session was closed (clean transport disconnect).
    SessionEnd(SessionEndEvent),
    /// Periodic health snapshot emitted by the health check loop.
    Heartbeat(HeartbeatEvent),
    /// A schema discovery response was captured (before proxy rewrite).
    SchemaCapture(SchemaCaptureEvent),
    /// Server indicated its schema changed (e.g., `notifications/tools/list_changed`).
    SchemaStale(SchemaStaleEvent),
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
    /// Wall-clock latency: proxy received request → sent response (ms).
    pub latency_ms: u64,
    /// Time spent waiting for upstream response (ms).
    pub upstream_ms: Option<u64>,
    /// Request payload size in bytes.
    pub request_size: Option<u64>,
    /// Response payload size in bytes.
    pub response_size: Option<u64>,

    /// MCP JSON-RPC error code (e.g., "-32600") if the response was an error.
    pub error_code: Option<String>,
    /// Error message (truncated to 512 chars).
    pub error_msg: Option<String>,

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

/// Captured MCP schema discovery response, emitted BEFORE proxy rewrite.
#[derive(Clone, Debug, Serialize)]
pub struct SchemaCaptureEvent {
    /// Unix milliseconds (UTC).
    pub ts: i64,
    /// Proxy name.
    pub proxy: String,
    /// Upstream MCP server URL.
    pub upstream_url: String,
    /// MCP method that produced this response (e.g., "initialize", "tools/list").
    pub method: String,
    /// The raw `result` field from the JSON-RPC response, serialized as JSON.
    pub payload: String,
    /// Pagination state — used by the writer to buffer multi-page responses.
    pub page_status: PageStatus,
}

/// Server indicated its schema changed (e.g., `notifications/tools/list_changed`).
#[derive(Clone, Debug, Serialize)]
pub struct SchemaStaleEvent {
    /// Unix milliseconds (UTC).
    pub ts: i64,
    /// Proxy name.
    pub proxy: String,
    /// Upstream MCP server URL.
    pub upstream_url: String,
    /// The method whose schema is now stale (e.g., "tools/list").
    pub method: String,
}

// ── Event sink trait ───────────────────────────────────────────────────

/// A sink that consumes proxy events from the event bus.
///
/// Register sinks at startup. The event bus calls `on_event` for every
/// event, and sinks filter by variant. Example:
///
/// ```rust,ignore
/// impl EventSink for PrometheusSink {
///     fn on_event(&self, event: &ProxyEvent) {
///         if let ProxyEvent::Request(e) = event {
///             self.request_counter.inc();
///             self.latency_histogram.observe(e.latency_ms as f64);
///         }
///     }
///     fn name(&self) -> &'static str { "prometheus" }
/// }
/// ```
///
/// # Contract
///
/// - **`on_event` must not block.** If the sink needs I/O (HTTP, disk),
///   buffer internally and flush in `flush()` or a background thread.
/// - **`on_batch`** is called when multiple events are available. Override
///   for sinks that benefit from batching (SQL INSERT, HTTP POST).
/// - **`flush`** is called periodically (~5s) and on graceful shutdown.
pub trait EventSink: Send + Sync {
    /// Process a single event. Must not block.
    fn on_event(&self, event: &ProxyEvent);

    /// Process a batch of events. Default calls `on_event` for each.
    fn on_batch(&self, events: &[ProxyEvent]) {
        for event in events {
            self.on_event(event);
        }
    }

    /// Flush internal buffers to their destination.
    fn flush(&self) {}

    /// Human-readable sink name (for logging and debugging).
    fn name(&self) -> &'static str;
}

/// A no-op sink that discards all events. Used when no sinks are configured.
pub struct NoopSink;

impl EventSink for NoopSink {
    fn on_event(&self, _event: &ProxyEvent) {}
    fn name(&self) -> &'static str {
        "noop"
    }
}
