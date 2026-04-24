//! Response-side middleware: serialize the buffered MCP message and
//! re-wrap as SSE if the upstream framing requires it.
//!
//! Replaces the inline serialize + `wrap_as_sse` dance at the end of
//! `handlers/buffered.rs`. Emits `Response::Raw` carrying the final
//! bytes and the correct `Content-Type` header, so the axum
//! `IntoResponse` edge (Phase 6) needs no discriminator beyond what is
//! already on the response.

use async_trait::async_trait;
use axum::body::Body;
use axum::http::HeaderValue;
use axum::http::header::CONTENT_TYPE;

use crate::proxy::pipeline::middleware::ResponseMiddleware;
use crate::proxy::pipeline::values::{Context, Envelope, Response};
use crate::proxy::sse::wrap_as_sse;

use super::shared;

pub struct EnvelopeSealMiddleware;

#[async_trait]
impl ResponseMiddleware for EnvelopeSealMiddleware {
    fn name(&self) -> &'static str {
        "envelope_seal"
    }

    async fn on_response(&self, resp: Response, _cx: &mut Context) -> Response {
        let Response::McpBuffered {
            envelope,
            message,
            status,
            mut headers,
        } = resp
        else {
            return resp;
        };

        let json_bytes = shared::serialize_envelope(&message.envelope);
        let (bytes, ct) = match envelope {
            Envelope::Json => (json_bytes, "application/json"),
            Envelope::Sse => (wrap_as_sse(&json_bytes), "text/event-stream"),
        };
        headers.insert(CONTENT_TYPE, HeaderValue::from_static(ct));
        Response::Raw {
            body: Body::from(bytes),
            status,
            headers,
        }
    }
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;

    use axum::http::{HeaderMap, StatusCode};
    use serde_json::Value;

    use crate::proxy::pipeline::envelope::JsonRpcEnvelope;
    use crate::proxy::pipeline::message::{McpMessage, MessageKind, ServerKind};
    use crate::proxy::pipeline::middlewares::test_support::{test_context, test_proxy_state};

    fn buffered(envelope: Envelope, body: &str) -> Response {
        let env = JsonRpcEnvelope::parse(body.as_bytes()).unwrap();
        let message = McpMessage {
            envelope: env,
            kind: MessageKind::Server(ServerKind::Result),
        };
        Response::McpBuffered {
            envelope,
            message,
            status: StatusCode::OK,
            headers: HeaderMap::new(),
        }
    }

    async fn body_bytes(resp: Response) -> (String, axum::http::HeaderMap, StatusCode) {
        match resp {
            Response::Raw {
                body,
                status,
                headers,
            } => {
                let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
                (String::from_utf8(bytes.to_vec()).unwrap(), headers, status)
            }
            _ => panic!("expected Raw"),
        }
    }

    #[tokio::test]
    async fn on_response__json_envelope_emits_raw_with_json_content_type() {
        let proxy = test_proxy_state();
        let mut cx = test_context(proxy);
        let resp = buffered(
            Envelope::Json,
            r#"{"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#,
        );

        let out = EnvelopeSealMiddleware.on_response(resp, &mut cx).await;
        let (body, headers, status) = body_bytes(out).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            headers.get(CONTENT_TYPE).unwrap().to_str().unwrap(),
            "application/json"
        );
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["jsonrpc"], "2.0");
        assert_eq!(v["id"], 1);
        assert_eq!(v["result"]["ok"], true);
    }

    #[tokio::test]
    async fn on_response__sse_envelope_wraps_as_event_stream() {
        let proxy = test_proxy_state();
        let mut cx = test_context(proxy);
        let resp = buffered(
            Envelope::Sse,
            r#"{"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#,
        );

        let out = EnvelopeSealMiddleware.on_response(resp, &mut cx).await;
        let (body, headers, _) = body_bytes(out).await;
        assert_eq!(
            headers.get(CONTENT_TYPE).unwrap().to_str().unwrap(),
            "text/event-stream"
        );
        assert!(body.starts_with("data: "), "got {body:?}");
        assert!(body.ends_with("\n\n"));
    }

    #[tokio::test]
    async fn on_response__error_envelope_preserves_code_and_message() {
        let proxy = test_proxy_state();
        let mut cx = test_context(proxy);
        let resp = buffered(
            Envelope::Json,
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32600,"message":"bad req"}}"#,
        );

        let out = EnvelopeSealMiddleware.on_response(resp, &mut cx).await;
        let (body, _, _) = body_bytes(out).await;
        let v: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["error"]["code"], -32600);
        assert_eq!(v["error"]["message"], "bad req");
    }

    #[tokio::test]
    async fn on_response__non_buffered_passthrough() {
        let proxy = test_proxy_state();
        let mut cx = test_context(proxy);
        let resp = Response::Upstream502 {
            reason: "boom".into(),
        };
        let out = EnvelopeSealMiddleware.on_response(resp, &mut cx).await;
        assert!(matches!(out, Response::Upstream502 { .. }));
    }

    #[tokio::test]
    async fn on_response__preserves_status_and_custom_headers() {
        let proxy = test_proxy_state();
        let mut cx = test_context(proxy);
        let env = JsonRpcEnvelope::parse(br#"{"jsonrpc":"2.0","id":1,"result":{}}"#).unwrap();
        let message = McpMessage {
            envelope: env,
            kind: MessageKind::Server(ServerKind::Result),
        };
        let mut headers = HeaderMap::new();
        headers.insert("x-trace-id", "abc".parse().unwrap());
        let resp = Response::McpBuffered {
            envelope: Envelope::Json,
            message,
            status: StatusCode::ACCEPTED,
            headers,
        };

        let out = EnvelopeSealMiddleware.on_response(resp, &mut cx).await;
        let (_, headers, status) = body_bytes(out).await;
        assert_eq!(status, StatusCode::ACCEPTED);
        assert_eq!(headers.get("x-trace-id").unwrap().to_str().unwrap(), "abc");
    }
}
