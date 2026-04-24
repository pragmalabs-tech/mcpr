//! The single I/O layer for the pipeline.
//!
//! Wraps `forward_request` from `proxy/forwarding.rs` and maps reqwest
//! failures to [`Response::Upstream502`]. Buffer vs stream decision
//! comes from the `Route` — no content-type sniffing at dispatch time.

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, Method, StatusCode, header};

use super::ProxyState;
use super::forwarding::{forward_request, read_body_capped};
use super::pipeline::driver::{StageGuard, Transport};
use super::pipeline::values::{
    BufferPolicy, Context, Envelope, McpRequest, RawRequest, Request, Response, Route, Working,
};
use super::sse::{extract_json_from_sse, split_upstream};
use crate::protocol::jsonrpc::JsonRpcEnvelope;
use crate::protocol::mcp::{McpMessage, MessageKind, classify_server};

pub struct ProxyTransport;

#[async_trait]
impl Transport for ProxyTransport {
    async fn dispatch(&self, req: Request, route: Route, cx: &mut Context) -> Response {
        let state = cx.intake.proxy.clone();
        match (req, route) {
            (
                Request::Mcp(mcp),
                Route::McpStreamableHttp {
                    upstream,
                    buffer_policy,
                    ..
                },
            ) => dispatch_mcp_post(state, mcp, upstream, buffer_policy, &mut cx.working).await,
            (Request::Mcp(mcp), Route::McpSseLegacy { upstream }) => {
                dispatch_sse_legacy(state, mcp, upstream, &mut cx.working).await
            }
            (Request::Raw(raw), Route::Raw { upstream }) => {
                dispatch_raw(state, raw, upstream, &mut cx.working).await
            }
            // Intake never produces `Request::OAuth` today; arm is defensive.
            (Request::OAuth(_), Route::Oauth { .. }) => Response::Upstream502 {
                reason: "oauth dispatch not implemented".into(),
            },
            _ => Response::Upstream502 {
                reason: "intake/router variant mismatch".into(),
            },
        }
    }
}

async fn dispatch_mcp_post(
    state: Arc<ProxyState>,
    mcp: McpRequest,
    upstream: String,
    buffer_policy: BufferPolicy,
    working: &mut Working,
) -> Response {
    let body_bytes = Bytes::from(mcp.envelope.to_bytes());
    let is_streaming = matches!(buffer_policy, BufferPolicy::Streamed);
    let upstream_started = Instant::now();
    let resp = match forward_request(
        &state.upstream,
        &upstream,
        Method::POST,
        &mcp.headers,
        &body_bytes,
        is_streaming,
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            working.upstream_us = Some(upstream_started.elapsed().as_micros() as u64);
            return Response::Upstream502 {
                reason: format!("{e}"),
            };
        }
    };
    working.upstream_us = Some(upstream_started.elapsed().as_micros() as u64);
    let status = resp.status();
    let headers = resp.headers().clone();

    match buffer_policy {
        BufferPolicy::Buffered { max } => {
            buffer_and_parse(resp, max, status, headers, working).await
        }
        BufferPolicy::Streamed => Response::McpStreamed {
            envelope: Envelope::Json,
            body: Body::from_stream(resp.bytes_stream()),
            status,
            headers,
        },
    }
}

async fn buffer_and_parse(
    resp: reqwest::Response,
    max: usize,
    status: StatusCode,
    headers: HeaderMap,
    working: &mut Working,
) -> Response {
    // `StageGuard` pushes a named timing on drop. Each wrapping block
    // scopes one phase; `?` / early returns still fire the guard's
    // Drop, so failure paths are measured too.
    let raw = {
        let _g = StageGuard::start("transport_buffer", &mut working.timings);
        match read_body_capped(resp, max).await {
            Ok(b) => b,
            Err(e) => {
                return Response::Upstream502 {
                    reason: e.to_string(),
                };
            }
        }
    };

    let was_sse = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|ct| ct.contains("text/event-stream"))
        .unwrap_or(false);
    let json_bytes: Vec<u8> = if was_sse {
        let _g = StageGuard::start("transport_sse_unwrap", &mut working.timings);
        extract_json_from_sse(&raw).unwrap_or_else(|| raw.to_vec())
    } else {
        raw.to_vec()
    };

    let envelope = {
        let _g = StageGuard::start("transport_json_parse", &mut working.timings);
        match JsonRpcEnvelope::parse(&json_bytes) {
            Ok(e) => e,
            Err(_) => {
                return Response::Raw {
                    body: Body::from(raw),
                    status,
                    headers,
                };
            }
        }
    };

    let kind = MessageKind::Server(classify_server(&envelope));
    let message = McpMessage { envelope, kind };
    Response::McpBuffered {
        envelope: if was_sse {
            Envelope::Sse
        } else {
            Envelope::Json
        },
        message,
        status,
        headers,
    }
}

async fn dispatch_sse_legacy(
    state: Arc<ProxyState>,
    mcp: McpRequest,
    upstream: String,
    working: &mut Working,
) -> Response {
    let empty = Bytes::new();
    let upstream_started = Instant::now();
    let resp = match forward_request(
        &state.upstream,
        &upstream,
        Method::GET,
        &mcp.headers,
        &empty,
        true,
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            working.upstream_us = Some(upstream_started.elapsed().as_micros() as u64);
            return Response::Upstream502 {
                reason: format!("{e}"),
            };
        }
    };
    working.upstream_us = Some(upstream_started.elapsed().as_micros() as u64);
    let status = resp.status();
    let headers = resp.headers().clone();
    Response::McpStreamed {
        envelope: Envelope::Sse,
        body: Body::from_stream(resp.bytes_stream()),
        status,
        headers,
    }
}

async fn dispatch_raw(
    state: Arc<ProxyState>,
    raw: RawRequest,
    upstream: String,
    working: &mut Working,
) -> Response {
    let (base, _) = split_upstream(&upstream);
    let url = format!("{}{}", base.trim_end_matches('/'), raw.path);
    // Passthrough does not cap the request body — `DefaultBodyLimit`
    // at the axum edge already rejected oversize requests, so everything
    // reaching here is within the configured limit.
    let body_bytes = axum::body::to_bytes(raw.body, usize::MAX)
        .await
        .unwrap_or_default();
    let upstream_started = Instant::now();
    let resp = match forward_request(
        &state.upstream,
        &url,
        raw.method,
        &raw.headers,
        &body_bytes,
        false,
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            working.upstream_us = Some(upstream_started.elapsed().as_micros() as u64);
            return Response::Upstream502 {
                reason: format!("{e}"),
            };
        }
    };
    working.upstream_us = Some(upstream_started.elapsed().as_micros() as u64);
    let status = resp.status();
    let headers = resp.headers().clone();
    let body_bytes = {
        let _g = StageGuard::start("transport_buffer", &mut working.timings);
        match read_body_capped(resp, state.max_response_body).await {
            Ok(b) => b,
            Err(e) => {
                return Response::Upstream502 {
                    reason: e.to_string(),
                };
            }
        }
    };
    Response::Raw {
        body: Body::from(body_bytes),
        status,
        headers,
    }
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;

    use std::sync::{Arc as StdArc, Mutex};
    use std::time::Duration;

    use axum::Router as AxumRouter;
    use axum::extract::State;
    use axum::http::{HeaderMap, HeaderValue, Request as AxumRequest, StatusCode};
    use axum::response::IntoResponse;
    use axum::routing::{any, post};
    use serde_json::Value;
    use tokio::net::TcpListener;

    use crate::protocol::mcp::{ClientMethod, ServerKind, ToolsMethod};
    use crate::proxy::pipeline::middlewares::test_support::{
        test_context, test_proxy_state_upstream,
    };
    use crate::proxy::pipeline::values::{McpTransport, RawRequest, Request};

    async fn spawn_upstream(app: AxumRouter) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    fn mcp_request(method: &str, body: &str, session: Option<&str>) -> McpRequest {
        let envelope = JsonRpcEnvelope::parse(body.as_bytes()).unwrap();
        let mut headers = HeaderMap::new();
        if let Some(sid) = session {
            headers.insert("mcp-session-id", HeaderValue::from_str(sid).unwrap());
        }
        McpRequest {
            transport: McpTransport::StreamableHttpPost,
            envelope,
            kind: crate::protocol::mcp::ClientKind::Request(ClientMethod::parse(method)),
            headers,
            session_hint: None,
        }
    }

    #[tokio::test]
    async fn dispatch__mcp_post_tools_list_buffered_returns_mcp_buffered_result() {
        let app = AxumRouter::new().route(
            "/mcp",
            post(|| async {
                (
                    StatusCode::OK,
                    [(axum::http::header::CONTENT_TYPE, "application/json")],
                    r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[{"name":"a"}]}}"#,
                )
            }),
        );
        let url = format!("{}/mcp", spawn_upstream(app).await);
        let proxy = test_proxy_state_upstream(url.clone());
        let mut cx = test_context(proxy);
        let req = mcp_request(
            "tools/list",
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
            None,
        );
        let route = Route::McpStreamableHttp {
            upstream: url,
            method: ClientMethod::Tools(ToolsMethod::List),
            buffer_policy: BufferPolicy::Buffered { max: 1 << 20 },
        };

        let out = ProxyTransport
            .dispatch(Request::Mcp(req), route, &mut cx)
            .await;
        match out {
            Response::McpBuffered {
                envelope, message, ..
            } => {
                assert_eq!(envelope, Envelope::Json);
                assert!(matches!(
                    message.kind,
                    MessageKind::Server(ServerKind::Result)
                ));
                let v: Value = message.envelope.result_as().unwrap();
                assert_eq!(v["tools"][0]["name"], "a");
            }
            other => panic!("expected McpBuffered, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn dispatch__mcp_post_buffered_sse_wrapped_response_unwraps_envelope_sse() {
        let app = AxumRouter::new().route(
            "/mcp",
            post(|| async {
                let body = format!(
                    "data: {}\n\n",
                    r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[]}}"#
                );
                (
                    StatusCode::OK,
                    [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                    body,
                )
            }),
        );
        let url = format!("{}/mcp", spawn_upstream(app).await);
        let proxy = test_proxy_state_upstream(url.clone());
        let mut cx = test_context(proxy);
        let req = mcp_request(
            "tools/list",
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
            None,
        );
        let route = Route::McpStreamableHttp {
            upstream: url,
            method: ClientMethod::Tools(ToolsMethod::List),
            buffer_policy: BufferPolicy::Buffered { max: 1 << 20 },
        };

        let out = ProxyTransport
            .dispatch(Request::Mcp(req), route, &mut cx)
            .await;
        match out {
            Response::McpBuffered { envelope, .. } => assert_eq!(envelope, Envelope::Sse),
            other => panic!("expected McpBuffered, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn dispatch__mcp_post_buffered_oversize_body_returns_502() {
        let app = AxumRouter::new().route(
            "/mcp",
            post(|| async {
                // 8 KiB body — content-length header is auto-populated,
                // so `read_body_capped` short-circuits on size mismatch.
                let body = vec![b'x'; 8 * 1024];
                (
                    StatusCode::OK,
                    [(axum::http::header::CONTENT_TYPE, "application/json")],
                    body,
                )
            }),
        );
        let url = format!("{}/mcp", spawn_upstream(app).await);
        let proxy = test_proxy_state_upstream(url.clone());
        let mut cx = test_context(proxy);
        let req = mcp_request(
            "tools/list",
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
            None,
        );
        let route = Route::McpStreamableHttp {
            upstream: url,
            method: ClientMethod::Tools(ToolsMethod::List),
            buffer_policy: BufferPolicy::Buffered { max: 1024 },
        };

        let out = ProxyTransport
            .dispatch(Request::Mcp(req), route, &mut cx)
            .await;
        assert!(matches!(out, Response::Upstream502 { .. }));
    }

    #[tokio::test]
    async fn dispatch__mcp_post_buffered_non_jsonrpc_falls_back_to_raw() {
        let app = AxumRouter::new().route(
            "/mcp",
            post(|| async {
                (
                    StatusCode::OK,
                    [(axum::http::header::CONTENT_TYPE, "text/html")],
                    "<!DOCTYPE html><html></html>",
                )
            }),
        );
        let url = format!("{}/mcp", spawn_upstream(app).await);
        let proxy = test_proxy_state_upstream(url.clone());
        let mut cx = test_context(proxy);
        let req = mcp_request(
            "tools/list",
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
            None,
        );
        let route = Route::McpStreamableHttp {
            upstream: url,
            method: ClientMethod::Tools(ToolsMethod::List),
            buffer_policy: BufferPolicy::Buffered { max: 1 << 20 },
        };

        let out = ProxyTransport
            .dispatch(Request::Mcp(req), route, &mut cx)
            .await;
        assert!(matches!(out, Response::Raw { .. }));
    }

    #[tokio::test]
    async fn dispatch__mcp_post_streamed_forwards_body_unchanged() {
        let app = AxumRouter::new().route(
            "/mcp",
            post(|| async {
                (
                    StatusCode::OK,
                    [(axum::http::header::CONTENT_TYPE, "application/json")],
                    r#"{"jsonrpc":"2.0","id":1,"result":{"pong":true}}"#,
                )
            }),
        );
        let url = format!("{}/mcp", spawn_upstream(app).await);
        let proxy = test_proxy_state_upstream(url.clone());
        let mut cx = test_context(proxy);
        let req = mcp_request("ping", r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#, None);
        let route = Route::McpStreamableHttp {
            upstream: url,
            method: ClientMethod::Ping,
            buffer_policy: BufferPolicy::Streamed,
        };

        let out = ProxyTransport
            .dispatch(Request::Mcp(req), route, &mut cx)
            .await;
        match out {
            Response::McpStreamed { body, status, .. } => {
                assert_eq!(status, StatusCode::OK);
                let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
                let s = std::str::from_utf8(&bytes).unwrap();
                assert!(s.contains("\"pong\":true"), "got {s}");
            }
            other => panic!("expected McpStreamed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn dispatch__mcp_sse_legacy_returns_streamed_sse() {
        let app = AxumRouter::new().route(
            "/mcp",
            any(|| async {
                (
                    StatusCode::OK,
                    [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                    "data: hi\n\n",
                )
            }),
        );
        let url = format!("{}/mcp", spawn_upstream(app).await);
        let proxy = test_proxy_state_upstream(url.clone());
        let mut cx = test_context(proxy);
        let req = McpRequest {
            transport: McpTransport::SseLegacyGet,
            envelope: JsonRpcEnvelope::parse(br#"{"jsonrpc":"2.0","method":"ping"}"#).unwrap(),
            kind: crate::protocol::mcp::ClientKind::Notification(
                crate::protocol::mcp::ClientNotifMethod::Unknown("ping".into()),
            ),
            headers: HeaderMap::new(),
            session_hint: None,
        };
        let route = Route::McpSseLegacy { upstream: url };

        let out = ProxyTransport
            .dispatch(Request::Mcp(req), route, &mut cx)
            .await;
        assert!(matches!(
            out,
            Response::McpStreamed {
                envelope: Envelope::Sse,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn dispatch__raw_appends_path_to_upstream_base() {
        #[derive(Clone)]
        struct Shared(StdArc<Mutex<Option<String>>>);
        let recorded = Shared(StdArc::new(Mutex::new(None)));
        let app = AxumRouter::new()
            .route(
                "/token",
                any(
                    |State(Shared(slot)): State<Shared>, req: AxumRequest<axum::body::Body>| async move {
                        *slot.lock().unwrap() = Some(req.uri().path().to_string());
                        (StatusCode::OK, "ok").into_response()
                    },
                ),
            )
            .with_state(recorded.clone());
        let base = spawn_upstream(app).await;
        let proxy = test_proxy_state_upstream(format!("{base}/mcp"));
        let mut cx = test_context(proxy);
        let req = RawRequest {
            method: Method::POST,
            path: "/token".into(),
            body: Body::from("grant_type=x"),
            headers: HeaderMap::new(),
        };
        let route = Route::Raw {
            upstream: format!("{base}/mcp"),
        };

        let out = ProxyTransport
            .dispatch(Request::Raw(req), route, &mut cx)
            .await;
        assert!(matches!(out, Response::Raw { .. }));
        assert_eq!(
            recorded.0.lock().unwrap().as_deref(),
            Some("/token"),
            "upstream should have seen /token",
        );
    }

    #[tokio::test]
    async fn dispatch__upstream_unreachable_is_502() {
        // Random unused port — nothing listening.
        let url = "http://127.0.0.1:1".to_string();
        let proxy = test_proxy_state_upstream(url.clone());
        let mut cx = test_context(proxy);
        let req = mcp_request(
            "tools/list",
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
            None,
        );
        let route = Route::McpStreamableHttp {
            upstream: url,
            method: ClientMethod::Tools(ToolsMethod::List),
            buffer_policy: BufferPolicy::Buffered { max: 1 << 20 },
        };

        let out = ProxyTransport
            .dispatch(Request::Mcp(req), route, &mut cx)
            .await;
        assert!(matches!(out, Response::Upstream502 { .. }));
    }

    #[tokio::test]
    async fn dispatch__variant_mismatch_is_502() {
        let proxy = test_proxy_state_upstream("http://unused.test".to_string());
        let mut cx = test_context(proxy);
        let req = mcp_request(
            "tools/list",
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
            None,
        );
        let route = Route::Raw {
            upstream: "http://unused.test".into(),
        };

        let out = ProxyTransport
            .dispatch(Request::Mcp(req), route, &mut cx)
            .await;
        assert!(matches!(out, Response::Upstream502 { reason } if reason.contains("mismatch")));
    }

    #[tokio::test]
    async fn dispatch__session_header_is_forwarded() {
        #[derive(Clone)]
        struct Shared(StdArc<Mutex<Option<String>>>);
        let recorded = Shared(StdArc::new(Mutex::new(None)));
        let app = AxumRouter::new()
            .route(
                "/mcp",
                post(
                    |State(Shared(slot)): State<Shared>, headers: HeaderMap| async move {
                        let sid = headers
                            .get("mcp-session-id")
                            .and_then(|v| v.to_str().ok())
                            .map(|s| s.to_string());
                        *slot.lock().unwrap() = sid;
                        (
                            StatusCode::OK,
                            [(axum::http::header::CONTENT_TYPE, "application/json")],
                            r#"{"jsonrpc":"2.0","id":1,"result":{}}"#,
                        )
                            .into_response()
                    },
                ),
            )
            .with_state(recorded.clone());
        let url = format!("{}/mcp", spawn_upstream(app).await);
        let proxy = test_proxy_state_upstream(url.clone());
        let mut cx = test_context(proxy);
        let req = mcp_request(
            "tools/list",
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
            Some("abc-123"),
        );
        let route = Route::McpStreamableHttp {
            upstream: url,
            method: ClientMethod::Tools(ToolsMethod::List),
            buffer_policy: BufferPolicy::Buffered { max: 1 << 20 },
        };

        let _ = ProxyTransport
            .dispatch(Request::Mcp(req), route, &mut cx)
            .await;
        // Give the upstream task a moment to observe (serve completes before dispatch returns).
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(
            recorded.0.lock().unwrap().as_deref(),
            Some("abc-123"),
            "upstream should have seen the mcp-session-id header",
        );
    }
}
