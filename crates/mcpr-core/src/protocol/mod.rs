//! # mcpr-protocol
//!
//! MCP specification layer: JSON-RPC 2.0, MCP message taxonomy, schema
//! capture primitives, session lifecycle, and the proxy's top-level
//! `Request` / `Result` taxonomy. The taxonomy is what the proxy accepts
//! at intake (axum) and what it observes coming back from upstream
//! (hyper-util's pooled HTTP/2 client); MCP traffic gets parsed into
//! JSON-RPC, everything else stays as raw HTTP.
//!
//! ## Module layout
//!
//! ```text
//! protocol/
//! +-- mcp.rs             JSON-RPC envelope + MCP 2025-11-25 method taxonomy
//! +-- http_request.rs    HTTP request/response aliases (axum::http::* over Bytes)
//! +-- schema.rs          Discovery type definitions + canonical hash view
//! +-- session.rs         Session lifecycle, SessionStore trait, MemorySessionStore
//! ```

pub mod http_request;
pub mod mcp;
pub mod schema;
pub mod session;
pub mod sse;

use axum::body::Bytes;
use axum::http::header::CONTENT_TYPE;
use axum::http::request::Parts as RequestParts;
use axum::http::response::Parts as ResponseParts;
use http_body_util::BodyExt;

/// Inbound traffic accepted by the proxy: a single MCP request, a
/// JSON-RPC batch (array on the wire), or raw HTTP. The MCP variants
/// carry the inbound HTTP `Parts` so headers (`mcp-session-id`, auth,
/// cookies, …) can be forwarded to the upstream.
#[derive(Clone)]
pub enum Request {
    Mcp(RequestParts, mcp::JsonRpcRequest),
    McpBatch(RequestParts, Vec<mcp::JsonRpcRequest>),
    Http(http_request::HttpRequest),
}

impl Request {
    /// Buffer the axum body once, then classify: a JSON body that parses
    /// as JSON-RPC becomes `Mcp` (object) or `McpBatch` (array); anything
    /// else is preserved as `Http`. The MCP variants keep the inbound
    /// `Parts` so downstream stages can forward request headers.
    pub async fn from_axum(req: axum::extract::Request) -> std::result::Result<Self, ParseError> {
        let (parts, body) = req.into_parts();
        let bytes = axum::body::to_bytes(body, usize::MAX)
            .await
            .map_err(ParseError::ReadBody)?;

        if is_json(parts.headers.get(CONTENT_TYPE)) {
            match json_shape(&bytes) {
                JsonShape::Array => {
                    if let Ok(batch) = serde_json::from_slice::<Vec<mcp::JsonRpcRequest>>(&bytes) {
                        return Ok(Self::McpBatch(parts, batch));
                    }
                }
                JsonShape::Object => {
                    if let Ok(rpc) = serde_json::from_slice::<mcp::JsonRpcRequest>(&bytes) {
                        return Ok(Self::Mcp(parts, rpc));
                    }
                }
                JsonShape::Other => {}
            }
        }
        Ok(Self::Http(axum::http::Request::from_parts(parts, bytes)))
    }
}

/// Outcome of an upstream call: a single JSON-RPC result, a batch (array
/// on the wire, paired 1:1 with `Request::McpBatch`), or a raw HTTP
/// response. The MCP variants carry the upstream's HTTP `Parts` so
/// response headers (`mcp-session-id`, `Set-Cookie`, …) flow back to
/// the client.
#[derive(Clone, Debug)]
pub enum Response {
    Mcp(ResponseParts, mcp::JsonRpcResult),
    McpBatch(ResponseParts, Vec<mcp::JsonRpcResult>),
    Http(http_request::Result),
}

impl Response {
    /// Buffer the hyper response once, then classify: a JSON body that
    /// parses as JSON-RPC becomes `Mcp` (object) or `McpBatch` (array);
    /// anything else is preserved as `Http`. Generic over the body type
    /// so it accepts `hyper::body::Incoming` from the production
    /// `hyper-util` pool and `Full<Bytes>` (or any `Body<Data = Bytes>`)
    /// from tests.
    pub async fn from_hyper<B>(resp: hyper::Response<B>) -> std::result::Result<Self, ParseError>
    where
        B: hyper::body::Body<Data = Bytes>,
        B::Error: std::error::Error + Send + Sync + 'static,
    {
        let (parts, body) = resp.into_parts();
        let bytes = body
            .collect()
            .await
            .map_err(|e| ParseError::ReadUpstream(Box::new(e)))?
            .to_bytes();

        if is_json(parts.headers.get(CONTENT_TYPE)) {
            match json_shape(&bytes) {
                JsonShape::Array => {
                    if let Ok(batch) = serde_json::from_slice::<Vec<mcp::JsonRpcResult>>(&bytes) {
                        return Ok(Self::McpBatch(parts, batch));
                    }
                }
                JsonShape::Object => {
                    if let Ok(rpc) = serde_json::from_slice::<mcp::JsonRpcResult>(&bytes) {
                        return Ok(Self::Mcp(parts, rpc));
                    }
                }
                JsonShape::Other => {}
            }
        }
        // SSE upstream: decode each `event: message` frame as a JSON-RPC
        // result. Non-result frames (e.g. intermediate notifications) don't
        // parse and are dropped here.
        //
        // SSE upstream: decode each `event: message` frame as a JSON-RPC
        // result. Non-result frames (e.g. intermediate notifications)
        // don't parse and are dropped here. The streaming path now lives
        // in `proxy2/stage/router_stage::classify_upstream`; this branch
        // is the buffered fallback for callers that go through
        // `Response::from_hyper` directly (e.g. the batch handler, which
        // forbids SSE anyway).
        if is_event_stream(parts.headers.get(CONTENT_TYPE)) {
            let frames = sse::decode_frames(&bytes);
            let results: Vec<mcp::JsonRpcResult> = frames
                .iter()
                .filter_map(|f| serde_json::from_slice::<mcp::JsonRpcResult>(f).ok())
                .collect();
            match results.len() {
                0 => {}
                1 => {
                    let mut results = results;
                    return Ok(Self::Mcp(parts, results.pop().unwrap()));
                }
                _ => return Ok(Self::McpBatch(parts, results)),
            }
        }
        Ok(Self::Http(axum::http::Response::from_parts(parts, bytes)))
    }
}

fn is_json(header: Option<&axum::http::HeaderValue>) -> bool {
    header
        .and_then(|v| v.to_str().ok())
        .is_some_and(|s| s.starts_with("application/json"))
}

pub(crate) fn is_event_stream(header: Option<&axum::http::HeaderValue>) -> bool {
    header
        .and_then(|v| v.to_str().ok())
        .is_some_and(|s| s.starts_with("text/event-stream"))
}

enum JsonShape {
    Object,
    Array,
    Other,
}

/// First non-whitespace byte decides single vs batch — JSON-RPC 2.0 §6.
fn json_shape(bytes: &[u8]) -> JsonShape {
    match bytes.iter().find(|b| !b.is_ascii_whitespace()) {
        Some(b'{') => JsonShape::Object,
        Some(b'[') => JsonShape::Array,
        _ => JsonShape::Other,
    }
}

#[derive(Debug)]
pub enum ParseError {
    ReadBody(axum::Error),
    ReadUpstream(Box<dyn std::error::Error + Send + Sync>),
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseError::ReadBody(e) => write!(f, "read inbound body: {e}"),
            ParseError::ReadUpstream(e) => write!(f, "read upstream body: {e}"),
        }
    }
}

impl std::error::Error for ParseError {}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{HeaderValue, Request as HttpReq, StatusCode};

    const RPC_REQUEST: &str = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
    const RPC_BATCH_REQUEST: &str = r#"[
        {"jsonrpc":"2.0","id":1,"method":"tools/list"},
        {"jsonrpc":"2.0","id":2,"method":"tools/list"}
    ]"#;
    const RPC_RESULT: &str = r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[]}}"#;
    const RPC_BATCH_RESULT: &str = r#"[
        {"jsonrpc":"2.0","id":1,"result":{"tools":[]}},
        {"jsonrpc":"2.0","id":2,"result":{"tools":[]}}
    ]"#;

    fn axum_post(content_type: &str, body: &str) -> axum::extract::Request {
        HttpReq::post("/")
            .header("content-type", content_type)
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    fn hyper_resp(
        status: u16,
        content_type: &str,
        body: &str,
    ) -> hyper::Response<http_body_util::Full<axum::body::Bytes>> {
        hyper::Response::builder()
            .status(status)
            .header("content-type", content_type)
            .body(http_body_util::Full::new(
                axum::body::Bytes::copy_from_slice(body.as_bytes()),
            ))
            .unwrap()
    }

    // ── is_json ───────────────────────────────────────────────

    #[test]
    fn is_json__application_json_returns_true() {
        assert!(is_json(Some(&HeaderValue::from_static("application/json"))));
    }

    #[test]
    fn is_json__with_charset_returns_true() {
        assert!(is_json(Some(&HeaderValue::from_static(
            "application/json; charset=utf-8"
        ))));
    }

    #[test]
    fn is_json__text_event_stream_returns_false() {
        assert!(!is_json(Some(&HeaderValue::from_static(
            "text/event-stream"
        ))));
    }

    #[test]
    fn is_json__missing_header_returns_false() {
        assert!(!is_json(None));
    }

    // ── json_shape ────────────────────────────────────────────

    #[test]
    fn json_shape__object() {
        assert!(matches!(json_shape(b"{}"), JsonShape::Object));
    }

    #[test]
    fn json_shape__array() {
        assert!(matches!(json_shape(b"[]"), JsonShape::Array));
    }

    #[test]
    fn json_shape__leading_whitespace_skipped() {
        assert!(matches!(json_shape(b"  \n\t{}"), JsonShape::Object));
        assert!(matches!(json_shape(b"\r\n  []"), JsonShape::Array));
    }

    #[test]
    fn json_shape__empty_returns_other() {
        assert!(matches!(json_shape(b""), JsonShape::Other));
        assert!(matches!(json_shape(b"   "), JsonShape::Other));
    }

    #[test]
    fn json_shape__non_json_returns_other() {
        assert!(matches!(json_shape(b"hello"), JsonShape::Other));
        assert!(matches!(json_shape(b"\"string\""), JsonShape::Other));
    }

    // ── Request::from_axum ────────────────────────────────────

    #[tokio::test]
    async fn request_from_axum__single_jsonrpc_returns_mcp() {
        let req = axum_post("application/json", RPC_REQUEST);
        let parsed = Request::from_axum(req).await.unwrap();
        assert!(matches!(parsed, Request::Mcp(_, _)));
    }

    #[tokio::test]
    async fn request_from_axum__jsonrpc_array_returns_mcp_batch() {
        let req = axum_post("application/json", RPC_BATCH_REQUEST);
        let parsed = Request::from_axum(req).await.unwrap();
        let Request::McpBatch(_, items) = parsed else {
            panic!("expected McpBatch");
        };
        assert_eq!(items.len(), 2);
    }

    #[tokio::test]
    async fn request_from_axum__mcp_preserves_inbound_headers() {
        let req = HttpReq::post("/")
            .header("content-type", "application/json")
            .header("mcp-session-id", "sess-xyz")
            .header("authorization", "Bearer abc")
            .body(Body::from(RPC_REQUEST.to_string()))
            .unwrap();
        let parsed = Request::from_axum(req).await.unwrap();
        let Request::Mcp(parts, _) = parsed else {
            panic!("expected Mcp");
        };
        assert_eq!(parts.headers["mcp-session-id"], "sess-xyz");
        assert_eq!(parts.headers["authorization"], "Bearer abc");
    }

    #[tokio::test]
    async fn request_from_axum__non_json_content_type_falls_back_to_http() {
        let req = axum_post("text/plain", RPC_REQUEST);
        let parsed = Request::from_axum(req).await.unwrap();
        let Request::Http(http_req) = parsed else {
            panic!("expected Http");
        };
        assert_eq!(http_req.body().as_ref(), RPC_REQUEST.as_bytes());
    }

    #[tokio::test]
    async fn request_from_axum__non_jsonrpc_json_falls_back_to_http() {
        let req = axum_post("application/json", r#"{"foo":"bar"}"#);
        let parsed = Request::from_axum(req).await.unwrap();
        assert!(matches!(parsed, Request::Http(_)));
    }

    #[tokio::test]
    async fn request_from_axum__malformed_jsonrpc_array_falls_back_to_http() {
        let req = axum_post("application/json", r#"[{"jsonrpc":"2.0","id":1}]"#);
        let parsed = Request::from_axum(req).await.unwrap();
        assert!(matches!(parsed, Request::Http(_)));
    }

    #[tokio::test]
    async fn request_from_axum__empty_body_falls_back_to_http() {
        let req = axum_post("application/json", "");
        let parsed = Request::from_axum(req).await.unwrap();
        assert!(matches!(parsed, Request::Http(_)));
    }

    #[tokio::test]
    async fn request_from_axum__http_preserves_method_and_headers() {
        let req = HttpReq::put("/path")
            .header("content-type", "text/plain")
            .header("x-trace-id", "abc123")
            .body(Body::from("payload"))
            .unwrap();
        let parsed = Request::from_axum(req).await.unwrap();
        let Request::Http(http_req) = parsed else {
            panic!("expected Http");
        };
        assert_eq!(http_req.method(), axum::http::Method::PUT);
        assert_eq!(http_req.uri().path(), "/path");
        assert_eq!(http_req.headers()["x-trace-id"], "abc123");
        assert_eq!(http_req.body().as_ref(), b"payload");
    }

    // ── Response::from_hyper ──────────────────────────────────

    #[tokio::test]
    async fn response_from_hyper__single_jsonrpc_returns_mcp() {
        let resp = hyper_resp(200, "application/json", RPC_RESULT);
        let parsed = Response::from_hyper(resp).await.unwrap();
        assert!(matches!(
            parsed,
            Response::Mcp(_, mcp::JsonRpcResult::Response(_))
        ));
    }

    #[tokio::test]
    async fn response_from_hyper__jsonrpc_array_returns_mcp_batch() {
        let resp = hyper_resp(200, "application/json", RPC_BATCH_RESULT);
        let parsed = Response::from_hyper(resp).await.unwrap();
        let Response::McpBatch(_, items) = parsed else {
            panic!("expected McpBatch");
        };
        assert_eq!(items.len(), 2);
    }

    #[tokio::test]
    async fn response_from_hyper__mcp_preserves_upstream_headers() {
        let resp = hyper::Response::builder()
            .status(200)
            .header("content-type", "application/json")
            .header("mcp-session-id", "sess-xyz")
            .header("set-cookie", "k=v; Path=/")
            .body(http_body_util::Full::new(
                axum::body::Bytes::copy_from_slice(RPC_RESULT.as_bytes()),
            ))
            .unwrap();
        let parsed = Response::from_hyper(resp).await.unwrap();
        let Response::Mcp(parts, _) = parsed else {
            panic!("expected Mcp");
        };
        assert_eq!(parts.headers["mcp-session-id"], "sess-xyz");
        assert_eq!(parts.headers["set-cookie"], "k=v; Path=/");
    }

    #[tokio::test]
    async fn response_from_hyper__non_json_content_type_falls_back_to_http() {
        let resp = hyper_resp(200, "text/html", "<html></html>");
        let parsed = Response::from_hyper(resp).await.unwrap();
        assert!(matches!(parsed, Response::Http(_)));
    }

    #[tokio::test]
    async fn response_from_hyper__sse_with_single_message_frame_returns_mcp() {
        let body = format!("event: message\ndata: {RPC_RESULT}\n\n");
        let resp = hyper_resp(200, "text/event-stream", &body);
        let parsed = Response::from_hyper(resp).await.unwrap();
        assert!(matches!(
            parsed,
            Response::Mcp(_, mcp::JsonRpcResult::Response(_))
        ));
    }

    #[tokio::test]
    async fn response_from_hyper__sse_with_multiple_message_frames_returns_mcp_batch() {
        let body = format!("data: {RPC_RESULT}\n\ndata: {RPC_RESULT}\n\n");
        let resp = hyper_resp(200, "text/event-stream", &body);
        let parsed = Response::from_hyper(resp).await.unwrap();
        let Response::McpBatch(_, items) = parsed else {
            panic!("expected McpBatch");
        };
        assert_eq!(items.len(), 2);
    }

    #[tokio::test]
    async fn response_from_hyper__sse_drops_non_message_frames_keeps_response() {
        let body = format!(
            "event: notifications/progress\ndata: {{\"x\":1}}\n\nevent: message\ndata: {RPC_RESULT}\n\n"
        );
        let resp = hyper_resp(200, "text/event-stream", &body);
        let parsed = Response::from_hyper(resp).await.unwrap();
        assert!(matches!(
            parsed,
            Response::Mcp(_, mcp::JsonRpcResult::Response(_))
        ));
    }

    #[tokio::test]
    async fn response_from_hyper__sse_preserves_upstream_headers() {
        let body = format!("data: {RPC_RESULT}\n\n");
        let resp = hyper::Response::builder()
            .status(200)
            .header("content-type", "text/event-stream")
            .header("mcp-session-id", "sess-xyz")
            .header("set-cookie", "k=v; Path=/")
            .body(http_body_util::Full::new(
                axum::body::Bytes::copy_from_slice(body.as_bytes()),
            ))
            .unwrap();
        let parsed = Response::from_hyper(resp).await.unwrap();
        let Response::Mcp(parts, _) = parsed else {
            panic!("expected Mcp");
        };
        assert_eq!(parts.headers["mcp-session-id"], "sess-xyz");
        assert_eq!(parts.headers["set-cookie"], "k=v; Path=/");
    }

    #[tokio::test]
    async fn response_from_hyper__sse_empty_body_falls_back_to_http() {
        let resp = hyper_resp(200, "text/event-stream", "");
        let parsed = Response::from_hyper(resp).await.unwrap();
        assert!(matches!(parsed, Response::Http(_)));
    }

    #[tokio::test]
    async fn response_from_hyper__sse_only_unparseable_frames_falls_back_to_http() {
        let resp = hyper_resp(200, "text/event-stream", "data: not-json\n\n");
        let parsed = Response::from_hyper(resp).await.unwrap();
        assert!(matches!(parsed, Response::Http(_)));
    }

    #[tokio::test]
    async fn response_from_hyper__http_preserves_status_and_headers() {
        let resp = hyper::Response::builder()
            .status(418)
            .header("content-type", "text/plain")
            .header("x-server", "teapot")
            .body(http_body_util::Full::new(axum::body::Bytes::from_static(
                b"short and stout",
            )))
            .unwrap();
        let parsed = Response::from_hyper(resp).await.unwrap();
        let Response::Http(http_resp) = parsed else {
            panic!("expected Http");
        };
        assert_eq!(http_resp.status(), StatusCode::IM_A_TEAPOT);
        assert_eq!(http_resp.headers()["x-server"], "teapot");
        assert_eq!(http_resp.body().as_ref(), b"short and stout");
    }
}
