pub mod csp;
pub mod proxy_config;
pub mod stage;
pub mod state;
pub mod upstream;

use std::sync::Arc;

use axum::{
    Router,
    extract::{Request as AxumRequest, State},
    response::{IntoResponse, Response as AxumResponse},
    routing::any,
};
use tower_http::cors::CorsLayer;

use crate::{
    env::Environment,
    event::EventBus,
    protocol::{Request, Response, is_event_stream, mcp::JsonRpcResult, sse},
    proxy2::stage::session_tracking_stage::SessionTrackingStage,
    timer::Timer,
};
use crate::{
    protocol::session::SessionStore,
    proxy2::{
        proxy_config::ProxyConfig,
        stage::{
            StagePipeline,
            csp_rewritten_stage::{CspRewriteConfig, CspRewritter},
            request_tracking_stage::ResponseLogStage,
            router_stage::{RouterOutput, RouterStage},
            schema_tracking_stage::SchemaTrackingStage,
            types::{RequestStage, ResponseStage},
        },
        state::InnerProxyState,
    },
};
use bytes::Bytes;
use futures_util::stream::{BoxStream, StreamExt};

/// Build an axum app that runs a single proxy from `cfg`. Every request
/// flows: axum → `Request::from_axum` → `StagePipeline::process` → axum
/// response. The CSP rewrite stage runs after the router so widget metas
/// in upstream responses are mutated before they reach the client.
pub fn build_app(cfg: Arc<ProxyConfig>, event_bus: EventBus) -> anyhow::Result<Router> {
    let cors = CorsLayer::permissive();

    // Build Stages
    let csp_rewritter = CspRewritter::new(CspRewriteConfig::from_proxy_config(&cfg));
    let router_stage = RouterStage::new(cfg)?;
    let request_stages: Vec<Box<dyn RequestStage>> = vec![];
    let response_stages: Vec<Box<dyn ResponseStage>> = vec![
        Box::new(SessionTrackingStage),
        Box::new(SchemaTrackingStage),
        Box::new(csp_rewritter),
        Box::new(ResponseLogStage),
    ];

    let pipeline = Arc::new(StagePipeline::new(
        request_stages,
        response_stages,
        router_stage,
        Arc::new(InnerProxyState::new(event_bus, SessionStore::new())),
    ));

    Ok(Router::new()
        .fallback(any(handle_request))
        .with_state(pipeline)
        .layer(cors))
}

async fn handle_request(
    State(pipeline): State<Arc<StagePipeline>>,
    req: AxumRequest,
) -> AxumResponse {
    if req.method() == axum::http::Method::GET
        && is_event_stream(req.headers().get(axum::http::header::ACCEPT))
    {
        let (parts, _body) = req.into_parts();
        return match pipeline.process_get_sse(parts).await {
            Ok(resp) => resp,
            Err(e) => (
                axum::http::StatusCode::BAD_GATEWAY,
                format!("upstream: {e}"),
            )
                .into_response(),
        };
    }

    let timer = Timer::new();

    let parse_id = timer.track_start("Parse");
    let parsed = match Request::from_axum(req).await {
        Ok(r) => r,
        Err(e) => {
            return (axum::http::StatusCode::BAD_REQUEST, format!("parse: {e}")).into_response();
        }
    };
    timer.track_end(parse_id);

    let result = pipeline.process(parsed, timer.clone()).await;

    let encode_id = timer.track_start("Encode");
    let resp = match result {
        Ok(out) => to_axum_response(out),
        Err(e) => (
            axum::http::StatusCode::BAD_GATEWAY,
            format!("upstream: {e}"),
        )
            .into_response(),
    };
    timer.track_end(encode_id);

    if Environment::debug() {
        eprintln!("[mcpr] timer:\n{}", timer);
    }
    resp
}

fn to_axum_response(output: RouterOutput) -> AxumResponse {
    match output {
        RouterOutput::Single(resp) => encode_single(resp),
        RouterOutput::Stream(parts, stream) => encode_stream(parts, stream),
    }
}

fn encode_single(resp: Response) -> AxumResponse {
    match resp {
        Response::Mcp(parts, result) => encode_mcp(parts, vec![result]),
        Response::McpBatch(parts, results) => encode_mcp(parts, results),
        Response::Http(http) => {
            let (parts, bytes) = http.into_parts();
            AxumResponse::from_parts(parts, axum::body::Body::from(bytes))
        }
    }
}

/// Stream JSON-RPC results back to the client as SSE: one frame per
/// yielded `Response::Mcp`, no buffering. Items that aren't `Mcp`
/// (router shouldn't produce any in a stream) become an io::Error,
/// which axum surfaces as a stream error and closes the body. Forwards
/// the upstream's response headers (`mcp-session-id`, `Set-Cookie`,
/// status, etc.) without waiting for the first frame, so session
/// negotiation works on the initial response.
fn encode_stream(
    mut parts: axum::http::response::Parts,
    stream: BoxStream<'static, anyhow::Result<Response>>,
) -> AxumResponse {
    parts.headers.remove(axum::http::header::CONTENT_LENGTH);
    parts.headers.remove(axum::http::header::TRANSFER_ENCODING);
    parts.headers.insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("text/event-stream"),
    );

    let body_stream = stream.map(|item| -> Result<Bytes, std::io::Error> {
        match item {
            Ok(Response::Mcp(_, result)) => Ok(sse::encode_one(&result)),
            Ok(_) => Err(std::io::Error::other("stream yielded non-Mcp Response")),
            Err(e) => Err(std::io::Error::other(e.to_string())),
        }
    });
    let body = axum::body::Body::from_stream(body_stream);

    let mut resp = AxumResponse::new(body);
    *resp.status_mut() = parts.status;
    let resp_headers = resp.headers_mut();
    for (name, value) in parts.headers.iter() {
        resp_headers.insert(name, value.clone());
    }
    resp
}

/// Re-emit MCP results to the client in the wire format upstream chose: SSE
/// frames if upstream returned `text/event-stream`, JSON otherwise. JSON
/// uses a single object for one result and an array for batches, matching
/// the wire shape of `Response::Mcp` vs `Response::McpBatch`.
fn encode_mcp(mut parts: axum::http::response::Parts, results: Vec<JsonRpcResult>) -> AxumResponse {
    let upstream_was_sse = is_event_stream(parts.headers.get(axum::http::header::CONTENT_TYPE));

    parts.headers.remove(axum::http::header::CONTENT_LENGTH);
    parts.headers.remove(axum::http::header::TRANSFER_ENCODING);

    let (content_type, body) = if upstream_was_sse {
        (
            axum::http::HeaderValue::from_static("text/event-stream"),
            axum::body::Body::from(sse::encode_results(&results)),
        )
    } else if results.len() == 1 {
        (
            axum::http::HeaderValue::from_static("application/json"),
            axum::body::Body::from(
                serde_json::to_vec(&results[0]).expect("JsonRpcResult serializes"),
            ),
        )
    } else {
        (
            axum::http::HeaderValue::from_static("application/json"),
            axum::body::Body::from(serde_json::to_vec(&results).expect("JsonRpcResult serializes")),
        )
    };
    parts
        .headers
        .insert(axum::http::header::CONTENT_TYPE, content_type);

    let mut resp = AxumResponse::new(body);
    *resp.status_mut() = parts.status;
    let resp_headers = resp.headers_mut();
    for (name, value) in parts.headers.iter() {
        resp_headers.insert(name, value.clone());
    }
    resp
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;

    use crate::protocol::mcp::{JsonRpcResponse, JsonRpcVersion, RequestId};
    use axum::http::HeaderValue;

    fn rpc_result(id: i64) -> JsonRpcResult {
        JsonRpcResult::Response(JsonRpcResponse {
            jsonrpc: JsonRpcVersion,
            id: RequestId::Number(id),
            result: Some(serde_json::json!({"ok": true})),
        })
    }

    fn parts_with(content_type: &str) -> axum::http::response::Parts {
        let mut parts = axum::http::Response::new(()).into_parts().0;
        parts
            .headers
            .insert("content-type", HeaderValue::from_str(content_type).unwrap());
        parts
    }

    async fn body_string(resp: AxumResponse) -> String {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    #[tokio::test]
    async fn to_axum_response__json_upstream_returns_application_json() {
        let resp = to_axum_response(RouterOutput::Single(Response::Mcp(
            parts_with("application/json"),
            rpc_result(1),
        )));
        assert_eq!(resp.headers()["content-type"], "application/json");
        let body = body_string(resp).await;
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["id"], 1);
    }

    #[tokio::test]
    async fn to_axum_response__json_upstream_batch_emits_array() {
        let resp = to_axum_response(RouterOutput::Single(Response::McpBatch(
            parts_with("application/json"),
            vec![rpc_result(1), rpc_result(2)],
        )));
        assert_eq!(resp.headers()["content-type"], "application/json");
        let body = body_string(resp).await;
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert!(v.is_array());
        assert_eq!(v.as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn to_axum_response__sse_upstream_returns_event_stream() {
        let resp = to_axum_response(RouterOutput::Single(Response::Mcp(
            parts_with("text/event-stream"),
            rpc_result(1),
        )));
        assert_eq!(resp.headers()["content-type"], "text/event-stream");
        let body = body_string(resp).await;
        assert!(body.starts_with("event: message\ndata: "));
        assert!(body.ends_with("\n\n"));
        assert_eq!(body.matches("\n\n").count(), 1);
    }

    #[tokio::test]
    async fn to_axum_response__sse_upstream_batch_emits_one_frame_per_result() {
        let resp = to_axum_response(RouterOutput::Single(Response::McpBatch(
            parts_with("text/event-stream"),
            vec![rpc_result(1), rpc_result(2), rpc_result(3)],
        )));
        assert_eq!(resp.headers()["content-type"], "text/event-stream");
        let body = body_string(resp).await;
        assert_eq!(body.matches("event: message\n").count(), 3);
        assert_eq!(body.matches("\n\n").count(), 3);
    }

    #[tokio::test]
    async fn to_axum_response__sse_upstream_roundtrips_through_decoder() {
        let original = vec![rpc_result(1), rpc_result(2)];
        let resp = to_axum_response(RouterOutput::Single(Response::McpBatch(
            parts_with("text/event-stream"),
            original.clone(),
        )));
        let body = body_string(resp).await;
        let frames = sse::decode_frames(body.as_bytes());
        assert_eq!(frames.len(), original.len());
        for (frame, expected) in frames.iter().zip(&original) {
            let parsed: JsonRpcResult = serde_json::from_slice(frame).unwrap();
            assert_eq!(
                serde_json::to_value(&parsed).unwrap(),
                serde_json::to_value(expected).unwrap()
            );
        }
    }

    #[tokio::test]
    async fn to_axum_response__strips_content_length_and_transfer_encoding() {
        let mut parts = parts_with("application/json");
        parts
            .headers
            .insert("content-length", HeaderValue::from_static("999"));
        parts
            .headers
            .insert("transfer-encoding", HeaderValue::from_static("chunked"));
        let resp = to_axum_response(RouterOutput::Single(Response::Mcp(parts, rpc_result(1))));
        assert!(!resp.headers().contains_key("transfer-encoding"));
        let cl = resp
            .headers()
            .get("content-length")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<usize>().ok());
        assert_ne!(cl, Some(999));
    }

    #[tokio::test]
    async fn to_axum_response__preserves_mcp_session_id() {
        let mut parts = parts_with("application/json");
        parts
            .headers
            .insert("mcp-session-id", HeaderValue::from_static("sess-xyz"));
        let resp = to_axum_response(RouterOutput::Single(Response::Mcp(parts, rpc_result(1))));
        assert_eq!(resp.headers()["mcp-session-id"], "sess-xyz");
    }

    // ── RouterOutput::Stream encoding ─────────────────────────

    /// Build a `RouterOutput::Stream` from a fixed list of `JsonRpcResult`s.
    fn stream_output(results: Vec<JsonRpcResult>) -> RouterOutput {
        stream_output_with_parts(parts_with("text/event-stream"), results)
    }

    fn stream_output_with_parts(
        parts: axum::http::response::Parts,
        results: Vec<JsonRpcResult>,
    ) -> RouterOutput {
        let parts_for_items = parts.clone();
        let stream = futures_util::stream::iter(
            results
                .into_iter()
                .map(move |r| Ok(Response::Mcp(parts_for_items.clone(), r))),
        );
        RouterOutput::Stream(parts, Box::pin(stream))
    }

    #[tokio::test]
    async fn to_axum_response__stream_emits_text_event_stream() {
        let resp = to_axum_response(stream_output(vec![rpc_result(1)]));
        assert_eq!(resp.headers()["content-type"], "text/event-stream");
    }

    #[tokio::test]
    async fn to_axum_response__stream_emits_one_frame_per_yielded_item() {
        let resp = to_axum_response(stream_output(vec![
            rpc_result(1),
            rpc_result(2),
            rpc_result(3),
        ]));
        let body = body_string(resp).await;
        assert_eq!(body.matches("event: message\n").count(), 3);
        assert_eq!(body.matches("\n\n").count(), 3);
    }

    #[tokio::test]
    async fn to_axum_response__stream_roundtrips_through_decoder() {
        let original = vec![rpc_result(1), rpc_result(2)];
        let resp = to_axum_response(stream_output(original.clone()));
        let body = body_string(resp).await;
        let frames = sse::decode_frames(body.as_bytes());
        assert_eq!(frames.len(), original.len());
        for (frame, expected) in frames.iter().zip(&original) {
            let parsed: JsonRpcResult = serde_json::from_slice(frame).unwrap();
            assert_eq!(
                serde_json::to_value(&parsed).unwrap(),
                serde_json::to_value(expected).unwrap()
            );
        }
    }

    #[tokio::test]
    async fn to_axum_response__stream_preserves_mcp_session_id() {
        // Regression: the Inspector observed "No valid session ID" because
        // the stream encoder was dropping upstream response headers. The
        // encoder must forward `mcp-session-id` (and other negotiation
        // headers) before the first frame so subsequent client requests
        // can use the session.
        let mut parts = parts_with("text/event-stream");
        parts
            .headers
            .insert("mcp-session-id", HeaderValue::from_static("sess-xyz"));
        parts
            .headers
            .insert("set-cookie", HeaderValue::from_static("k=v; Path=/"));
        let resp = to_axum_response(stream_output_with_parts(parts, vec![rpc_result(1)]));
        assert_eq!(resp.headers()["mcp-session-id"], "sess-xyz");
        assert_eq!(resp.headers()["set-cookie"], "k=v; Path=/");
        assert_eq!(resp.headers()["content-type"], "text/event-stream");
    }

    #[tokio::test]
    async fn to_axum_response__stream_strips_content_length_and_transfer_encoding() {
        let mut parts = parts_with("text/event-stream");
        parts
            .headers
            .insert("content-length", HeaderValue::from_static("999"));
        parts
            .headers
            .insert("transfer-encoding", HeaderValue::from_static("chunked"));
        let resp = to_axum_response(stream_output_with_parts(parts, vec![rpc_result(1)]));
        assert!(!resp.headers().contains_key("transfer-encoding"));
        let cl = resp
            .headers()
            .get("content-length")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<usize>().ok());
        assert_ne!(cl, Some(999));
    }

    // ── handle_request: GET SSE branching ─────────────────────

    use crate::event::EventBus;
    use crate::proxy2::csp::CspConfig;
    use crate::proxy2::stage::router_stage::RouterStage;
    use axum::extract::{Request as AxumRequestExtract, State as AxumState};
    use axum::routing::get as axum_get;

    fn pipeline_for(url: &str) -> Arc<StagePipeline> {
        let cfg = Arc::new(ProxyConfig {
            name: "test".into(),
            mcp: url.to_string(),
            port: None,
            csp: CspConfig::default(),
            max_request_body_size: None,
            max_response_body_size: None,
            max_concurrent_upstream: None,
            connect_timeout: None,
            request_timeout: None,
        });
        Arc::new(StagePipeline::new(
            vec![],
            vec![],
            RouterStage::new(cfg).unwrap(),
            Arc::new(InnerProxyState::new(
                EventBus::for_tests(),
                SessionStore::new(),
            )),
        ))
    }

    async fn spawn_upstream(router: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        format!("http://{addr}")
    }

    fn get_request(uri: &str, accept: Option<&str>) -> AxumRequestExtract {
        let mut builder = axum::http::Request::builder().method("GET").uri(uri);
        if let Some(a) = accept {
            builder = builder.header("accept", a);
        }
        builder.body(axum::body::Body::empty()).unwrap()
    }

    async fn spawn_get_sse_upstream() -> String {
        async fn handler() -> AxumResponse {
            let body = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/tools/list_changed\"}\n\n";
            AxumResponse::builder()
                .status(200)
                .header("content-type", "text/event-stream")
                .body(axum::body::Body::from(body))
                .unwrap()
        }
        spawn_upstream(Router::new().route("/", axum_get(handler))).await
    }

    #[tokio::test]
    async fn handle_request__get_with_event_stream_accept_opens_sse() {
        let url = spawn_get_sse_upstream().await;
        let pipeline = pipeline_for(&url);

        let req = get_request("/", Some("text/event-stream"));
        let resp = handle_request(AxumState(pipeline), req).await;

        assert_eq!(resp.headers()["content-type"], "text/event-stream");
        let body = body_string(resp).await;
        assert!(body.contains("notifications/tools/list_changed"));
    }

    #[tokio::test]
    async fn handle_request__plain_get_falls_back_to_http_path() {
        // Plain GET (no SSE Accept) goes through Request::from_axum →
        // Request::Http → handle_http_request, which forwards verbatim.
        // Upstream returns plain text; if the SSE branch had taken it,
        // content-type would be text/event-stream.
        async fn handler() -> AxumResponse {
            AxumResponse::builder()
                .status(200)
                .header("content-type", "text/plain")
                .body(axum::body::Body::from("hello"))
                .unwrap()
        }
        let url = spawn_upstream(Router::new().route("/", axum_get(handler))).await;
        let pipeline = pipeline_for(&url);

        let req = get_request("/", None);
        let resp = handle_request(AxumState(pipeline), req).await;

        assert_eq!(resp.status(), 200);
        assert_eq!(resp.headers()["content-type"], "text/plain");
        assert_eq!(body_string(resp).await, "hello");
    }

    #[tokio::test]
    async fn handle_request__get_sse_is_path_agnostic() {
        // Inbound URI path is irrelevant — dispatch is method + Accept.
        // Upstream URL determines where the proxy sends; the inbound path
        // is not inspected.
        let url = spawn_get_sse_upstream().await;
        let pipeline = pipeline_for(&url);

        let req = get_request("/anything/at/all", Some("text/event-stream"));
        let resp = handle_request(AxumState(pipeline), req).await;

        assert_eq!(resp.headers()["content-type"], "text/event-stream");
    }
}
