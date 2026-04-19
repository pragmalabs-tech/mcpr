//! Per-request context threaded through the pipeline.
//!
//! [`RequestContext`] is a one-pass parse of the incoming HTTP request
//! that every downstream stage reads from instead of re-parsing.
//! Handlers update a small set of mutable fields (session id after
//! upstream assigns one, client name/version after session lookup)
//! before handing off to [`super::emit::emit_request_event`].
//!
//! Response-side state now lives as local variables inside each handler
//! in [`super::handlers`] — there's no shared `ResponseContext` blob
//! passed between stages anymore.

use std::time::Instant;

use crate::protocol::session::ClientInfo;
use crate::protocol::{McpMethod, ParsedBody};
use axum::http::Method;

pub struct RequestContext {
    pub start: Instant,

    // ── HTTP ──
    pub http_method: Method,
    pub path: String,
    pub request_size: usize,
    pub wants_sse: bool,

    // ── Session (set from header; overwritten when upstream assigns one) ──
    pub session_id: Option<String>,

    // ── JSON-RPC / MCP (None when the body is not JSON-RPC) ──
    pub jsonrpc: Option<ParsedBody>,
    pub mcp_method: Option<McpMethod>,
    /// String form for event output. Set to the protocol method for MCP POSTs
    /// and overwritten by specific handlers where appropriate (e.g. "SSE").
    pub mcp_method_str: Option<String>,
    /// `tools/call` tool name; `None` for other methods.
    pub tool: Option<String>,
    pub is_batch: bool,

    // ── Client info ──
    /// Parsed from `initialize` params. The Initialize success path stores
    /// this into the session store.
    pub client_info_from_init: Option<ClientInfo>,
    /// Resolved from the session store by the handler before emit.
    pub client_name: Option<String>,
    pub client_version: Option<String>,

    /// Transform tags pushed by handlers. The emit stage joins them
    /// with `+` to build the `RequestEvent.note` field
    /// (e.g. `["rewritten", "sse"]` → `"rewritten+sse"`).
    pub tags: Vec<&'static str>,
}
