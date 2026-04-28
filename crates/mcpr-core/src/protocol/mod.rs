//! # mcpr-protocol
//!
//! MCP specification layer: JSON-RPC 2.0, MCP message taxonomy, schema
//! capture primitives, session lifecycle, and the proxy's top-level
//! `Request` / `Result` taxonomy. The taxonomy is what the proxy accepts
//! at intake (axum) and what it observes coming back from upstream
//! (reqwest); MCP traffic gets parsed into JSON-RPC, everything else
//! stays as raw HTTP.
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

use axum::http::header::CONTENT_TYPE;

/// Inbound traffic accepted by the proxy: a single MCP request, a
/// JSON-RPC batch (array on the wire), or raw HTTP.
pub enum Request {
    Mcp(mcp::JsonRpcRequest),
    McpBatch(Vec<mcp::JsonRpcRequest>),
    Http(http_request::HttpRequest),
}

impl Request {
    /// Buffer the axum body once, then classify: a JSON body that parses
    /// as JSON-RPC becomes `Mcp` (object) or `McpBatch` (array); anything
    /// else is preserved as `Http`.
    pub async fn from_axum(req: axum::extract::Request) -> std::result::Result<Self, ParseError> {
        let (parts, body) = req.into_parts();
        let bytes = axum::body::to_bytes(body, usize::MAX)
            .await
            .map_err(ParseError::ReadBody)?;

        if is_json(parts.headers.get(CONTENT_TYPE)) {
            match json_shape(&bytes) {
                JsonShape::Array => {
                    if let Ok(batch) = serde_json::from_slice::<Vec<mcp::JsonRpcRequest>>(&bytes) {
                        return Ok(Self::McpBatch(batch));
                    }
                }
                JsonShape::Object => {
                    if let Ok(rpc) = serde_json::from_slice::<mcp::JsonRpcRequest>(&bytes) {
                        return Ok(Self::Mcp(rpc));
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
/// response.
pub enum Result {
    Mcp(mcp::JsonRpcResult),
    McpBatch(Vec<mcp::JsonRpcResult>),
    Http(http_request::Result),
}

impl Result {
    /// Buffer the reqwest response once, then classify: a JSON body that
    /// parses as JSON-RPC becomes `Mcp` (object) or `McpBatch` (array);
    /// anything else is preserved as `Http`.
    pub async fn from_reqwest(resp: reqwest::Response) -> std::result::Result<Self, ParseError> {
        let status = resp.status();
        let version = resp.version();
        let headers = resp.headers().clone();
        let bytes = resp.bytes().await.map_err(ParseError::ReadUpstream)?;

        if is_json(headers.get(CONTENT_TYPE)) {
            match json_shape(&bytes) {
                JsonShape::Array => {
                    if let Ok(batch) = serde_json::from_slice::<Vec<mcp::JsonRpcResult>>(&bytes) {
                        return Ok(Self::McpBatch(batch));
                    }
                }
                JsonShape::Object => {
                    if let Ok(rpc) = serde_json::from_slice::<mcp::JsonRpcResult>(&bytes) {
                        return Ok(Self::Mcp(rpc));
                    }
                }
                JsonShape::Other => {}
            }
        }
        let mut http_resp = axum::http::Response::new(bytes);
        *http_resp.status_mut() = status;
        *http_resp.version_mut() = version;
        *http_resp.headers_mut() = headers;
        Ok(Self::Http(http_resp))
    }
}

fn is_json(header: Option<&axum::http::HeaderValue>) -> bool {
    header
        .and_then(|v| v.to_str().ok())
        .is_some_and(|s| s.starts_with("application/json"))
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
    ReadUpstream(reqwest::Error),
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
    use axum::http::{HeaderValue, Request as HttpReq, Response as HttpResp, StatusCode};

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

    fn reqwest_resp(status: u16, content_type: &str, body: &str) -> reqwest::Response {
        let resp = HttpResp::builder()
            .status(status)
            .header("content-type", content_type)
            .body(body.as_bytes().to_vec())
            .unwrap();
        reqwest::Response::from(resp)
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
        assert!(matches!(parsed, Request::Mcp(_)));
    }

    #[tokio::test]
    async fn request_from_axum__jsonrpc_array_returns_mcp_batch() {
        let req = axum_post("application/json", RPC_BATCH_REQUEST);
        let parsed = Request::from_axum(req).await.unwrap();
        let Request::McpBatch(items) = parsed else {
            panic!("expected McpBatch");
        };
        assert_eq!(items.len(), 2);
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

    // ── Result::from_reqwest ──────────────────────────────────

    #[tokio::test]
    async fn result_from_reqwest__single_jsonrpc_returns_mcp() {
        let resp = reqwest_resp(200, "application/json", RPC_RESULT);
        let parsed = Result::from_reqwest(resp).await.unwrap();
        assert!(matches!(
            parsed,
            Result::Mcp(mcp::JsonRpcResult::Response(_))
        ));
    }

    #[tokio::test]
    async fn result_from_reqwest__jsonrpc_array_returns_mcp_batch() {
        let resp = reqwest_resp(200, "application/json", RPC_BATCH_RESULT);
        let parsed = Result::from_reqwest(resp).await.unwrap();
        let Result::McpBatch(items) = parsed else {
            panic!("expected McpBatch");
        };
        assert_eq!(items.len(), 2);
    }

    #[tokio::test]
    async fn result_from_reqwest__non_json_content_type_falls_back_to_http() {
        let resp = reqwest_resp(200, "text/html", "<html></html>");
        let parsed = Result::from_reqwest(resp).await.unwrap();
        assert!(matches!(parsed, Result::Http(_)));
    }

    #[tokio::test]
    async fn result_from_reqwest__sse_content_type_falls_back_to_http() {
        let resp = reqwest_resp(200, "text/event-stream", "data: hi\n\n");
        let parsed = Result::from_reqwest(resp).await.unwrap();
        assert!(matches!(parsed, Result::Http(_)));
    }

    #[tokio::test]
    async fn result_from_reqwest__http_preserves_status_and_headers() {
        let resp = HttpResp::builder()
            .status(418)
            .header("content-type", "text/plain")
            .header("x-server", "teapot")
            .body(b"short and stout".to_vec())
            .unwrap();
        let parsed = Result::from_reqwest(reqwest::Response::from(resp))
            .await
            .unwrap();
        let Result::Http(http_resp) = parsed else {
            panic!("expected Http");
        };
        assert_eq!(http_resp.status(), StatusCode::IM_A_TEAPOT);
        assert_eq!(http_resp.headers()["x-server"], "teapot");
        assert_eq!(http_resp.body().as_ref(), b"short and stout");
    }
}
