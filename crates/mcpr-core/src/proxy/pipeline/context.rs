//! Per-request contexts threaded through the pipeline.
//!
//! [`RequestContext`] is a one-pass parse of the incoming HTTP request that
//! every downstream stage reads from instead of re-parsing. Handlers update
//! a small set of mutable fields (session id after upstream assigns one,
//! client name/version after session lookup) before handing off to
//! [`super::emit::emit_request_event`].
//!
//! [`ResponseContext`] carries the accumulating response through the
//! response middleware chain: the raw body, an optional parsed JSON view,
//! SSE-wrapping state, and JSON-RPC error info. Middleware mutate it; the
//! final handler builds the `axum::Response` from `resp.body` + `resp.headers`.

use std::time::Instant;

use crate::protocol::session::ClientInfo;
use crate::protocol::{McpMethod, ParsedBody};
use axum::http::{HeaderMap, Method};
use serde_json::Value;

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

    /// Transform tags pushed by handlers / middleware. The emit stage joins
    /// them with `+` to build the `RequestEvent.note` field
    /// (e.g. `["rewritten", "sse"]` → `"rewritten+sse"`).
    pub tags: Vec<&'static str>,
}

/// Response-side state threaded through the response middleware chain.
///
/// Middleware mutate `body` and `json` in place. Handlers instantiate after
/// reading the upstream body and finalize by building an `axum::Response` from
/// `(status, headers, body)`.
pub struct ResponseContext {
    pub status: u16,
    pub headers: HeaderMap,
    /// Serialized response body — what gets returned to the client. Held
    /// verbatim from the upstream until `EncodeResponseJson` overwrites it.
    /// When no middleware mutates `json`, this retains the original bytes
    /// byte-for-byte (preserving SSE framing, key order, etc.).
    pub body: Vec<u8>,
    /// True when the upstream sent SSE-wrapped JSON. `DecodeResponseJson`
    /// sets it; `EncodeResponseJson` reads it to decide whether to re-wrap.
    pub was_sse: bool,
    /// Parsed JSON view of the body. Populated by `DecodeResponseJson`;
    /// mutated by later middleware; serialized back into `body` by
    /// `EncodeResponseJson` only when `json_mutated` is set.
    pub json: Option<Value>,
    /// Signals that some middleware mutated `json`. Set by any middleware
    /// that takes `json.as_mut()` or reassigns `json`. `EncodeResponseJson`
    /// skips re-serialization when false, leaving `body` untouched — the
    /// byte-pass fast path.
    pub json_mutated: bool,
    /// JSON-RPC error extracted from `json` (when present).
    pub rpc_error: Option<(i64, String)>,
    pub upstream_us: Option<u64>,
}

impl ResponseContext {
    pub fn new(status: u16, headers: HeaderMap, body: Vec<u8>, upstream_us: Option<u64>) -> Self {
        Self {
            status,
            headers,
            body,
            was_sse: false,
            json: None,
            json_mutated: false,
            rpc_error: None,
            upstream_us,
        }
    }

    /// Mutable access to the parsed JSON value, marking it as mutated so
    /// `EncodeResponseJson` will re-serialize. Prefer this over direct
    /// `json.as_mut()` — forgetting to set the flag causes silent staleness.
    pub fn json_mut(&mut self) -> Option<&mut Value> {
        if self.json.is_some() {
            self.json_mutated = true;
        }
        self.json.as_mut()
    }
}
