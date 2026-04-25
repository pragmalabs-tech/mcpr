//! Top-level value types for the pipeline.
//!
//! See `PIPELINE.md` §Types. These are the sum types
//! passed between pipeline stages: `Request` in, `Response` out, with
//! `Context` threaded by reference.

use std::fmt;
use std::sync::Arc;
use std::time::Instant;

use axum::{
    body::{Body, Bytes},
    http::{HeaderMap, HeaderValue, Method, StatusCode, header::CONTENT_TYPE},
    response::IntoResponse,
};

use crate::protocol::session::{ClientInfo, SessionInfo};
use crate::proxy::ProxyState;
use crate::proxy::forwarding::build_response;
use crate::proxy::sse::wrap_as_sse;

use crate::protocol::jsonrpc::JsonRpcEnvelope;
use crate::protocol::mcp::{ClientKind, ClientMethod, McpMessage};

use super::stubs::{OAuthKind, SessionId, TagSet, UrlMap};

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
/// middleware. `impl IntoResponse for Response` (below) converts this
/// into an axum response at the edge.
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
    /// other response — `HealthTrack` records it, `emit` tags the event
    /// as `upstream error`, and `IntoResponse` renders a 502.
    Upstream502 { reason: String },
}

/// Framing of a buffered MCP response. Data, not control flow — the
/// final `EnvelopeSeal` stage applies the wrap once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Envelope {
    Json,
    Sse,
}

impl IntoResponse for Response {
    fn into_response(self) -> axum::response::Response {
        match self {
            Response::Raw {
                body,
                status,
                headers,
            } => build_response(status.as_u16(), &headers, body),
            Response::McpStreamed {
                body,
                status,
                headers,
                ..
            } => build_response(status.as_u16(), &headers, body),
            Response::McpBuffered {
                envelope: env,
                message,
                status,
                mut headers,
            } => {
                let json_bytes = message.envelope.to_bytes();
                let (bytes, ct) = match env {
                    Envelope::Json => (json_bytes, "application/json"),
                    Envelope::Sse => (wrap_as_sse(&json_bytes), "text/event-stream"),
                };
                headers.insert(CONTENT_TYPE, HeaderValue::from_static(ct));
                build_response(status.as_u16(), &headers, Body::from(bytes))
            }
            Response::OauthJson {
                doc,
                status,
                mut headers,
            } => {
                let bytes = serde_json::to_vec(&doc).unwrap_or_default();
                headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
                build_response(status.as_u16(), &headers, Body::from(bytes))
            }
            Response::Upstream502 { reason } => {
                (StatusCode::BAD_GATEWAY, format!("Upstream error: {reason}")).into_response()
            }
        }
    }
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
    pub session: Option<SessionInfo>,
    pub client: Option<ClientInfo>,
    /// Originating client method, stashed on the request side so
    /// response-side middlewares know what produced the response.
    pub request_method: Option<ClientMethod>,
    /// Tool name for `tools/call`, stashed on the request side so the
    /// emitter can populate `RequestEvent.tool` without re-parsing.
    pub request_tool: Option<String>,
    /// Resource URI for `resources/{read,subscribe,unsubscribe}`. Feeds
    /// `RequestEvent.resource_uri`.
    pub request_resource_uri: Option<String>,
    /// Prompt name for `prompts/get`. Feeds `RequestEvent.prompt_name`.
    pub request_prompt_name: Option<String>,
    /// Serialized response body size in bytes. `EnvelopeSealMiddleware`
    /// fills this on the buffered path; streaming paths leave it `None`.
    /// Feeds `RequestEvent.response_size`.
    pub response_size: Option<u64>,
    /// Wall-clock time spent in `forward_request` (network RTT +
    /// upstream work). Populated by `ProxyTransport`. Feeds
    /// `RequestEvent.upstream_us`.
    pub upstream_us: Option<u64>,
    pub tags: TagSet,
    /// Per-stage wall-clock timings, pushed in order as each stage
    /// completes. Populated only when `MCPR_STAGE_TIMING` is set —
    /// otherwise stays empty. Feeds `RequestEvent.stage_timings`.
    pub timings: Vec<StageTiming>,
}

/// One named wall-clock measurement for a single pipeline stage.
///
/// Each middleware and named non-middleware site (intake parse,
/// transport upstream/buffer/unwrap/parse) pushes one of these onto
/// `Working.timings` when stage timing is enabled. The driver sums
/// nothing — duplicates are fine; aggregators (e.g. the bench
/// diagnostic) group by `name`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct StageTiming {
    pub name: &'static str,
    pub elapsed_us: u64,
}
