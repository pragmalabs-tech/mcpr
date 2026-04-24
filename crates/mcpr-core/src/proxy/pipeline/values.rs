//! Top-level value types for the target pipeline.
//!
//! See `PIPELINE_ARCHITECTURE.md` §Types. These are the sum types
//! passed between pipeline stages: `Request` in, `Response` out, with
//! `Context` threaded by reference.

use std::fmt;
use std::sync::Arc;
use std::time::Instant;

use axum::{
    body::{Body, Bytes},
    http::{HeaderMap, Method, StatusCode},
};

use crate::event::types::StageTimings;
use crate::protocol::session::ClientInfo;
use crate::proxy::ProxyState;

use super::envelope::JsonRpcEnvelope;
use super::message::{ClientKind, ClientMethod, McpMessage};
use super::stubs::{OAuthKind, SessionId, SessionRecord, TagSet, UrlMap};

// ── Request side ─────────────────────────────────────────────

/// Top-level sum type produced by intake. Owns its body.
#[derive(Debug)]
pub enum Request {
    /// JSON-RPC 2.0 over streamable HTTP or legacy SSE.
    Mcp(McpRequest),
    /// OAuth / discovery / token / callback — content-matched.
    OAuth(OAuthRequest),
    /// Everything else — forwarded unchanged, no inspection.
    Raw(RawRequest),
}

/// An MCP HTTP request from the client. Body carries exactly one
/// JSON-RPC message — no batching per MCP 2025-11-25.
#[derive(Debug)]
pub struct McpRequest {
    pub transport: McpTransport,
    pub envelope: JsonRpcEnvelope,
    pub kind: ClientKind,
    pub headers: HeaderMap,
    pub session_hint: Option<SessionId>,
}

/// Which MCP transport the request is using. Streamable HTTP is the
/// primary path; legacy HTTP+SSE is supported but demoted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpTransport {
    /// POST body carries a client→server message; response is a single
    /// JSON body or a chunked stream of messages.
    StreamableHttpPost,
    /// GET that opens a server-push stream — used for server→client
    /// messages outside the request/response pattern.
    StreamableHttpGet,
    /// Legacy HTTP+SSE: GET with `Accept: text/event-stream`.
    SseLegacyGet,
}

#[derive(Debug)]
pub struct OAuthRequest {
    pub kind: OAuthKind,
    pub body: Bytes,
    pub headers: HeaderMap,
}

#[derive(Debug)]
pub struct RawRequest {
    pub method: Method,
    pub path: String,
    pub body: Body,
    pub headers: HeaderMap,
}

// ── Response side ────────────────────────────────────────────

/// Sum type produced by the transport, or by a short-circuiting
/// middleware. `IntoResponse` (Phase 6) converts this into an axum
/// response at the edge.
#[derive(Debug)]
pub enum Response {
    /// Buffered MCP response: one parsed `McpMessage`, mutated in place
    /// by response middlewares, serialized once by `EnvelopeSeal`.
    McpBuffered {
        envelope: Envelope,
        message: McpMessage,
        status: StatusCode,
        headers: HeaderMap,
    },
    /// Streamed MCP response: bytes forwarded as-is. Content-touching
    /// middlewares do not fire on this variant.
    McpStreamed {
        envelope: Envelope,
        body: Body,
        status: StatusCode,
        headers: HeaderMap,
    },
    /// OAuth discovery / token JSON — a parsed document that
    /// `UrlMapMiddleware` rewrites before `IntoResponse`.
    OauthJson {
        doc: serde_json::Value,
        status: StatusCode,
        headers: HeaderMap,
    },
    /// Forwarded raw body — no inspection.
    Raw {
        body: Body,
        status: StatusCode,
        headers: HeaderMap,
    },
    /// Upstream failure. Travels through the response chain like any
    /// other response; replaces today's ad-hoc `emit_upstream_error`.
    Upstream502 { reason: String },
}

/// Framing of a buffered MCP response. Data, not control flow — the
/// final `EnvelopeSeal` stage applies the wrap once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Envelope {
    Json,
    Sse,
}

// ── Route ────────────────────────────────────────────────────

/// Output of the router. Declarative; no I/O.
#[derive(Debug, Clone)]
pub enum Route {
    McpStreamableHttp {
        upstream: String,
        method: ClientMethod,
        buffer_policy: BufferPolicy,
    },
    McpSseLegacy {
        upstream: String,
    },
    Oauth {
        upstream: String,
        rewrite: UrlMap,
    },
    Raw {
        upstream: String,
    },
}

/// Whether the transport should collect the upstream body or forward
/// bytes as they arrive. Owned by the routing table, not by the
/// protocol enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BufferPolicy {
    Streamed,
    Buffered { max: usize },
}

// ── Context ──────────────────────────────────────────────────

/// Per-request state carried by reference through both chains. Split so
/// the type system distinguishes immutable-after-intake fields from
/// mutable working state.
#[derive(Debug)]
pub struct Context {
    pub intake: Intake,
    pub working: Working,
}

/// Set once at intake, read many times. Changing anything here after
/// intake is a type error.
pub struct Intake {
    pub start: Instant,
    pub proxy: Arc<ProxyState>,
    pub http_method: Method,
    pub path: String,
    pub request_size: usize,
}

// `ProxyState` does not implement `Debug`. Print its name and skip its
// internals so `Intake` can still be logged/asserted on in tests.
impl fmt::Debug for Intake {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Intake")
            .field("start", &self.start)
            .field("proxy", &"Arc<ProxyState>")
            .field("http_method", &self.http_method)
            .field("path", &self.path)
            .field("request_size", &self.request_size)
            .finish()
    }
}

/// Mutated by middlewares as they learn about the request. Final
/// contents feed the `Emit` stage.
#[derive(Debug, Default)]
pub struct Working {
    pub session: Option<SessionRecord>,
    pub client: Option<ClientInfo>,
    pub tags: TagSet,
    pub timings: StageTimings,
}
