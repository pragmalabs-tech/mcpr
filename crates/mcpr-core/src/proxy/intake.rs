//! Content-based classification of axum request parts into the
//! [`Request`] sum type.
//!
//! Rules:
//!
//! 1. `POST` + body parses as JSON-RPC → `Request::Mcp` with
//!    `transport: StreamableHttpPost`.
//! 2. `GET` + `Accept: text/event-stream` → `Request::Mcp` with
//!    `transport: SseLegacyGet` (synthetic envelope; downstream matches
//!    on transport variant, not envelope contents).
//! 3. `DELETE` + `mcp-session-id` header → `Request::Mcp` with a
//!    synthetic envelope so `SessionDeleteMiddleware` can pattern-match
//!    on `Request::Mcp`.
//! 4. Everything else → `Request::Raw`.
//!
//! OAuth classification is deferred. Non-MCP traffic becomes
//! `Request::Raw`; `UrlMapMiddleware` rewrites JSON Raw bodies.

use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, Method, Uri, header};

use super::pipeline::stubs::SessionId;
use super::pipeline::values::{McpRequest, McpTransport, RawRequest, Request};
use crate::protocol::jsonrpc::JsonRpcRequest;
use crate::protocol::mcp::{ClientKind, ClientNotifMethod, classify_client};

pub fn from_axum_parts(method: Method, headers: HeaderMap, uri: Uri, body: Bytes) -> Request {
    let path = uri.path().to_string();

    // MCP POST — JSON-RPC body parse succeeds.
    if method == Method::POST
        && let Ok(envelope) = JsonRpcRequest::parse(&body)
    {
        let kind = classify_client(&envelope);
        let session_hint = session_hint_from_headers(&headers);
        return Request::Mcp(McpRequest {
            transport: McpTransport::StreamableHttpPost,
            envelope,
            kind,
            headers,
            session_hint,
        });
    }

    // Legacy SSE GET — `Accept: text/event-stream` opens a server-push
    // stream. Envelope is synthetic; downstream matches on transport.
    if method == Method::GET && wants_sse(&headers) {
        let envelope = JsonRpcRequest::parse(br#"{"jsonrpc":"2.0","method":"ping"}"#)
            .expect("static synthetic envelope parses");
        let kind = ClientKind::Notification(ClientNotifMethod::Unknown("ping".into()));
        let session_hint = session_hint_from_headers(&headers);
        return Request::Mcp(McpRequest {
            transport: McpTransport::SseLegacyGet,
            envelope,
            kind,
            headers,
            session_hint,
        });
    }

    // Session DELETE — empty-body DELETE + `mcp-session-id`.
    if method == Method::DELETE
        && let Some(sid_value) = headers.get("mcp-session-id").cloned()
    {
        let envelope = JsonRpcRequest::parse(br#"{"jsonrpc":"2.0","method":"delete"}"#)
            .expect("static synthetic envelope parses");
        let kind = ClientKind::Notification(ClientNotifMethod::Unknown("delete".into()));
        let session_hint = sid_value
            .to_str()
            .ok()
            .map(|s| SessionId::new(s.to_string()));
        return Request::Mcp(McpRequest {
            transport: McpTransport::StreamableHttpPost,
            envelope,
            kind,
            headers,
            session_hint,
        });
    }

    Request::Raw(RawRequest {
        method,
        path,
        body: Body::from(body),
        headers,
    })
}

fn wants_sse(headers: &HeaderMap) -> bool {
    headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .map(|a| a.contains("text/event-stream"))
        .unwrap_or(false)
}

fn session_hint_from_headers(headers: &HeaderMap) -> Option<SessionId> {
    headers
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .map(|s| SessionId::new(s.to_string()))
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;

    use crate::protocol::mcp::{ClientMethod, LifecycleMethod, ToolsMethod};

    fn uri(path: &str) -> Uri {
        path.parse().unwrap()
    }

    fn headers_with(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                v.parse().unwrap(),
            );
        }
        h
    }

    #[test]
    fn from_axum_parts__post_tools_list_is_mcp_streamable() {
        let req = from_axum_parts(
            Method::POST,
            HeaderMap::new(),
            uri("/mcp"),
            Bytes::from_static(br#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#),
        );
        let Request::Mcp(mcp) = req else {
            panic!("expected Mcp");
        };
        assert_eq!(mcp.transport, McpTransport::StreamableHttpPost);
        assert_eq!(
            mcp.kind,
            ClientKind::Request(ClientMethod::Tools(ToolsMethod::List))
        );
    }

    #[test]
    fn from_axum_parts__session_header_populates_hint() {
        let req = from_axum_parts(
            Method::POST,
            headers_with(&[("mcp-session-id", "abc")]),
            uri("/mcp"),
            Bytes::from_static(br#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#),
        );
        let Request::Mcp(mcp) = req else {
            panic!("expected Mcp");
        };
        assert_eq!(mcp.session_hint.map(|s| s.0), Some("abc".into()));
    }

    #[test]
    fn from_axum_parts__post_invalid_json_falls_through_to_raw() {
        let req = from_axum_parts(
            Method::POST,
            HeaderMap::new(),
            uri("/mcp"),
            Bytes::from_static(b"not json"),
        );
        assert!(matches!(req, Request::Raw(_)));
    }

    #[test]
    fn from_axum_parts__post_valid_json_but_not_jsonrpc_falls_through_to_raw() {
        let req = from_axum_parts(
            Method::POST,
            HeaderMap::new(),
            uri("/"),
            Bytes::from_static(br#"{"foo":"bar"}"#),
        );
        assert!(matches!(req, Request::Raw(_)));
    }

    #[test]
    fn from_axum_parts__get_with_sse_accept_is_sse_legacy() {
        let req = from_axum_parts(
            Method::GET,
            headers_with(&[("accept", "text/event-stream")]),
            uri("/mcp"),
            Bytes::new(),
        );
        let Request::Mcp(mcp) = req else {
            panic!("expected Mcp");
        };
        assert_eq!(mcp.transport, McpTransport::SseLegacyGet);
    }

    #[test]
    fn from_axum_parts__get_without_sse_is_raw() {
        let req = from_axum_parts(Method::GET, HeaderMap::new(), uri("/health"), Bytes::new());
        assert!(matches!(req, Request::Raw(_)));
    }

    #[test]
    fn from_axum_parts__delete_with_session_id_is_mcp() {
        let req = from_axum_parts(
            Method::DELETE,
            headers_with(&[("mcp-session-id", "abc")]),
            uri("/mcp"),
            Bytes::new(),
        );
        let Request::Mcp(mcp) = req else {
            panic!("expected Mcp");
        };
        assert_eq!(mcp.session_hint.map(|s| s.0), Some("abc".into()));
        assert_eq!(mcp.transport, McpTransport::StreamableHttpPost);
    }

    #[test]
    fn from_axum_parts__delete_without_session_id_is_raw() {
        let req = from_axum_parts(Method::DELETE, HeaderMap::new(), uri("/mcp"), Bytes::new());
        assert!(matches!(req, Request::Raw(_)));
    }

    #[test]
    fn from_axum_parts__notification_classified_as_notification() {
        let req = from_axum_parts(
            Method::POST,
            HeaderMap::new(),
            uri("/mcp"),
            Bytes::from_static(br#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#),
        );
        let Request::Mcp(mcp) = req else {
            panic!("expected Mcp");
        };
        assert_eq!(
            mcp.kind,
            ClientKind::Notification(ClientNotifMethod::Initialized)
        );
    }

    #[test]
    fn from_axum_parts__initialize_request_stays_mcp() {
        let req = from_axum_parts(
            Method::POST,
            HeaderMap::new(),
            uri("/mcp"),
            Bytes::from_static(br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#),
        );
        let Request::Mcp(mcp) = req else {
            panic!("expected Mcp");
        };
        assert_eq!(
            mcp.kind,
            ClientKind::Request(ClientMethod::Lifecycle(LifecycleMethod::Initialize))
        );
    }
}
