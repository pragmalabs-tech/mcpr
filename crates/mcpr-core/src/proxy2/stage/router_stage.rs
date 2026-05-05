//! Terminal stage: forwards `Request` to upstream via the sharded
//! HTTP/2 client pool and turns the upstream answer into a
//! [`RouterOutput`]: either a `Single(Response)` (JSON / non-SSE
//! upstreams, batches, raw HTTP) or a `Stream` of `Response::Mcp`
//! frames (SSE upstreams). The pipeline runs response stages on the
//! `Single` value or per yielded item on the `Stream`; `to_axum_response`
//! encodes accordingly.
//!
//! MCP requests are sharded by JSON-RPC id; raw HTTP requests pick a
//! random shard since they have no session identity.

use std::convert::Infallible;
use std::sync::Arc;

use axum::body::Body as AxumBody;
use axum::http::header::CONTENT_TYPE;
use axum::http::request::Parts as RequestParts;
use axum::http::response::Parts as ResponseParts;
use axum::response::Response as AxumResponse;
use bytes::Bytes;
use futures_util::stream::{BoxStream, StreamExt};
use http::{HeaderMap, HeaderName};
use http_body_util::{BodyDataStream, BodyExt, Full, combinators::BoxBody};
use hyper::Method;

use crate::{
    auth::{OAuthRequest, OAuthResponse},
    protocol::{
        Request, Response,
        http_request::HttpRequest,
        is_event_stream,
        mcp::{JsonRpcRequest, JsonRpcResult},
        session::session_id_from_headers,
        sse,
    },
    proxy2::{proxy_config::ProxyConfig, state::ProxyState, upstream::pool::UpstreamPool},
};

/// Shape of an upstream response after `RouterStage` decides whether to
/// buffer it or stream it. `Single` carries a fully-collected
/// [`Response`]; `Stream` carries a chunked-passthrough sequence of
/// `Response::Mcp(parts, result)` items, one per dispatched
/// `event: message` SSE frame, plus the upstream `ResponseParts` so the
/// encoder can forward response headers (`mcp-session-id`, `Set-Cookie`,
/// status, etc.) without waiting for the first frame.
pub enum RouterOutput {
    Single(Response),
    Stream(ResponseParts, BoxStream<'static, anyhow::Result<Response>>),
}

/// Hop-by-hop headers (RFC 7230 §6.1) that must not cross the proxy boundary.
const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailers",
    "transfer-encoding",
    "upgrade",
];

/// Headers the proxy controls itself; never copied through.
fn is_proxy_managed(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "host" | "content-length" | "content-type" | "accept"
    )
}

fn is_hop_by_hop(name: &HeaderName) -> bool {
    HOP_BY_HOP.contains(&name.as_str())
}

pub struct RouterStage {
    pool: UpstreamPool,
}

impl RouterStage {
    pub fn new(cfg: Arc<ProxyConfig>) -> anyhow::Result<Self> {
        let pool = UpstreamPool::new(cfg)?;
        Ok(Self { pool })
    }
}

impl RouterStage {
    pub async fn process(
        &self,
        request: Request,
        state: ProxyState,
    ) -> anyhow::Result<RouterOutput> {
        match request {
            Request::Mcp(parts, req) => handle_mcp_request(parts, req, &state, &self.pool).await,
            Request::McpBatch(parts, reqs) => {
                handle_mcp_requests(parts, reqs, &state, &self.pool).await
            }
            Request::OAuth(req) => handle_oauth_request(req, &state, &self.pool).await,
            Request::Http(req) => handle_http_request(req, &state, &self.pool).await,
        }
    }

    /// Open an upstream `GET` SSE stream for the inbound `mcp-session-id`
    /// and pipe its body straight back to the client. Bypasses the
    /// `Response` enum and the response stage chain: notifications and
    /// server-initiated requests don't fit `JsonRpcResult`, so frames flow
    /// as raw bytes. Sharded by session id (or a per-stream UUID if the
    /// header is absent) so all traffic for a session rides one H2
    /// connection.
    pub async fn open_get_sse(&self, parts: RequestParts) -> anyhow::Result<AxumResponse> {
        let session_id = session_id_from_headers(&parts.headers)
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

        let mut builder = hyper::Request::builder()
            .method(Method::GET)
            .uri(self.pool.upstream().url.as_str())
            .header(hyper::header::ACCEPT, "text/event-stream");
        builder = forward_request_headers(builder, &parts.headers);
        let upstream_req = builder.body(box_full(Bytes::new()))?;

        let resp = self
            .pool
            .pick(session_id.as_str())
            .request(upstream_req)
            .await?;
        Ok(passthrough_sse(resp))
    }
}

/// Forward an upstream SSE response body through to the client. Strips
/// hop-by-hop headers (notably `connection: close`, which would make the
/// client tear down its socket after headers) and `content-length` (axum
/// sets framing itself for streaming bodies). Status and remaining headers
/// — `content-type`, `mcp-session-id`, `set-cookie`, `cache-control` — flow
/// through unchanged.
fn passthrough_sse(resp: hyper::Response<hyper::body::Incoming>) -> AxumResponse {
    let (mut parts, body) = resp.into_parts();
    let to_remove: Vec<HeaderName> = parts
        .headers
        .keys()
        .filter(|name| is_hop_by_hop(name))
        .cloned()
        .collect();
    for name in to_remove {
        parts.headers.remove(&name);
    }
    parts.headers.remove(axum::http::header::CONTENT_LENGTH);

    let body_stream = BodyDataStream::new(body)
        .map(|chunk| chunk.map_err(|e| std::io::Error::other(e.to_string())));
    let body = AxumBody::from_stream(body_stream);

    let mut resp = AxumResponse::new(body);
    *resp.status_mut() = parts.status;
    *resp.headers_mut() = parts.headers;
    resp
}

async fn handle_mcp_requests(
    parts: RequestParts,
    requests: Vec<JsonRpcRequest>,
    _state: &ProxyState,
    pool: &UpstreamPool,
) -> anyhow::Result<RouterOutput> {
    // JSON-RPC batches map to a JSON array on the wire. SSE upstreams
    // are rejected per item: a stream doesn't fit array semantics.
    let mut results = Vec::with_capacity(requests.len());
    let mut last_parts: Option<ResponseParts> = None;

    for request in requests {
        let upstream_req = build_mcp_upstream_request(pool, &parts.headers, &request)?;
        let resp = pool.pick(&request.id).request(upstream_req).await?;
        let (rp, result) = match Response::from_hyper(resp).await? {
            Response::Mcp(parts, result) => (parts, result),
            Response::McpBatch(_, _) => {
                return Err(anyhow::anyhow!(
                    "upstream returned a JSON-RPC batch for a single MCP request"
                ));
            }
            Response::Http(http) => {
                return Err(anyhow::anyhow!(
                    "upstream returned non-JSON-RPC body (status {})",
                    http.status()
                ));
            }
        };
        results.push(result);
        last_parts = Some(rp);
    }

    let envelope = last_parts.unwrap_or_else(empty_response_parts);
    Ok(RouterOutput::Single(Response::McpBatch(envelope, results)))
}

async fn handle_mcp_request(
    parts: RequestParts,
    request: JsonRpcRequest,
    _state: &ProxyState,
    pool: &UpstreamPool,
) -> anyhow::Result<RouterOutput> {
    let upstream_req = build_mcp_upstream_request(pool, &parts.headers, &request)?;
    let resp = pool.pick(&request.id).request(upstream_req).await?;
    let output = classify_upstream(resp).await?;
    // Upstream returning a batch for a single-MCP request is malformed.
    if let RouterOutput::Single(Response::McpBatch(_, _)) = &output {
        return Err(anyhow::anyhow!(
            "upstream returned a JSON-RPC batch for a single MCP request"
        ));
    }
    if let RouterOutput::Single(Response::Http(http)) = &output {
        return Err(anyhow::anyhow!(
            "upstream returned non-JSON-RPC body (status {})",
            http.status()
        ));
    }
    Ok(output)
}

fn build_mcp_upstream_request(
    pool: &UpstreamPool,
    inbound_headers: &HeaderMap,
    request: &JsonRpcRequest,
) -> anyhow::Result<hyper::Request<BoxBody<Bytes, hyper::Error>>> {
    let body = Bytes::from(serde_json::to_vec(request)?);
    let mut builder = hyper::Request::builder()
        .method(Method::POST)
        .uri(pool.upstream().url.as_str())
        .header(hyper::header::CONTENT_TYPE, "application/json")
        .header(hyper::header::ACCEPT, "application/json, text/event-stream");
    builder = forward_request_headers(builder, inbound_headers);
    Ok(builder.body(box_full(body))?)
}

/// Classify an upstream HTTP response. SSE upstreams return
/// [`RouterOutput::Stream`] without buffering, so frames flow as upstream
/// emits them. Everything else collects via [`Response::from_hyper`] and
/// returns [`RouterOutput::Single`].
///
/// SSE frames that don't deserialize as `JsonRpcResult` (notifications,
/// server-initiated requests) are dropped via `filter_map`. TODO:
/// preserve them once we add `Response::Notification` /
/// `Response::ServerRequest` variants.
async fn classify_upstream<B>(resp: hyper::Response<B>) -> anyhow::Result<RouterOutput>
where
    B: hyper::body::Body<Data = Bytes> + Send + Unpin + 'static,
    B::Error: std::error::Error + Send + Sync + 'static,
{
    let (parts, body) = resp.into_parts();

    if is_event_stream(parts.headers.get(CONTENT_TYPE)) {
        let parts_for_items = parts.clone();
        let frame_stream = sse::decode_frame_stream(body);
        let response_stream = frame_stream.filter_map(move |frame_or_err| {
            let parts = parts_for_items.clone();
            async move {
                match frame_or_err {
                    Ok(bytes) => serde_json::from_slice::<JsonRpcResult>(&bytes)
                        .ok()
                        .map(|result| Ok(Response::Mcp(parts, result))),
                    Err(e) => Some(Err(anyhow::anyhow!("upstream stream error: {e}"))),
                }
            }
        });
        return Ok(RouterOutput::Stream(parts, Box::pin(response_stream)));
    }

    let resp = hyper::Response::from_parts(parts, body);
    Ok(RouterOutput::Single(Response::from_hyper(resp).await?))
}

/// Copy inbound headers onto the upstream request, skipping hop-by-hop and
/// proxy-managed headers (Host/Content-Length/Content-Type/Accept). This
/// preserves session, auth, and custom headers transparently.
fn forward_request_headers(
    mut builder: hyper::http::request::Builder,
    inbound: &HeaderMap,
) -> hyper::http::request::Builder {
    for (name, value) in inbound.iter() {
        if is_hop_by_hop(name) || is_proxy_managed(name) {
            continue;
        }
        builder = builder.header(name, value);
    }
    builder
}

fn empty_response_parts() -> ResponseParts {
    axum::http::Response::new(()).into_parts().0
}

/// Handle a `Request::OAuth` by consulting the configured
/// [`crate::auth::AuthProvider`]. Provider returns:
/// - `Serve(json)` -> respond inline with 200 + JSON.
/// - `NotFound`     -> respond inline with 404.
/// - `Forward` (or no provider configured) -> behave exactly like
///   `Request::Http` and forward to upstream.
async fn handle_oauth_request(
    req: OAuthRequest,
    state: &ProxyState,
    pool: &UpstreamPool,
) -> anyhow::Result<RouterOutput> {
    let response = state.auth_provider.as_ref().map(|p| p.handle(&req));
    match response {
        Some(OAuthResponse::Serve(body)) => {
            Ok(RouterOutput::Single(Response::Http(serve_json_200(body))))
        }
        Some(OAuthResponse::NotFound) => Ok(RouterOutput::Single(Response::Http(serve_status(
            axum::http::StatusCode::NOT_FOUND,
        )))),
        None | Some(OAuthResponse::Forward) => handle_http_request(req.http, state, pool).await,
    }
}

fn serve_json_200(body: serde_json::Value) -> axum::http::Response<Bytes> {
    let bytes = serde_json::to_vec(&body).unwrap_or_default();
    let mut resp = axum::http::Response::new(Bytes::from(bytes));
    *resp.status_mut() = axum::http::StatusCode::OK;
    resp.headers_mut().insert(
        CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/json"),
    );
    resp
}

fn serve_status(status: axum::http::StatusCode) -> axum::http::Response<Bytes> {
    let mut resp = axum::http::Response::new(Bytes::new());
    *resp.status_mut() = status;
    resp
}

async fn handle_http_request(
    request: HttpRequest,
    _state: &ProxyState,
    pool: &UpstreamPool,
) -> anyhow::Result<RouterOutput> {
    let shard_id = uuid::Uuid::new_v4();

    let (parts, body) = request.into_parts();

    let mut builder = hyper::Request::builder()
        .method(parts.method.clone())
        .uri(pool.upstream().url.as_str());

    for key in [
        hyper::header::AUTHORIZATION,
        hyper::header::CONTENT_TYPE,
        hyper::header::ACCEPT,
    ] {
        if let Some(v) = parts.headers.get(&key) {
            builder = builder.header(key, v);
        }
    }
    for key in ["mcp-session-id", "last-event-id"] {
        if let Some(v) = parts.headers.get(key) {
            builder = builder.header(key, v);
        }
    }

    let upstream_req = builder.body(box_full(body))?;
    let resp = pool.pick(&shard_id).request(upstream_req).await?;
    Ok(RouterOutput::Single(Response::from_hyper(resp).await?))
}

fn box_full(bytes: Bytes) -> BoxBody<Bytes, hyper::Error> {
    Full::new(bytes)
        .map_err(|never: Infallible| match never {})
        .boxed()
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;
    use crate::{
        protocol::mcp::{
            ClientMethod, JsonRpcRequest, JsonRpcResult, JsonRpcVersion, RequestId, ToolsMethod,
        },
        proxy2::state::InnerProxyState,
    };
    use axum::{
        Router,
        body::Bytes as AxumBytes,
        extract::State as AxumState,
        http::HeaderMap,
        response::IntoResponse,
        routing::{get as axum_get, post},
    };
    use serde_json::{Value, json};
    use std::sync::{Arc, Mutex};

    // ── Helpers ───────────────────────────────────────────────

    fn config_for(url: &str) -> Arc<ProxyConfig> {
        Arc::new(ProxyConfig::for_tests(url))
    }

    fn state() -> ProxyState {
        InnerProxyState::for_tests()
    }

    fn mcp_request(id: i64) -> JsonRpcRequest {
        JsonRpcRequest {
            jsonrpc: JsonRpcVersion,
            id: RequestId::Number(id),
            method: ClientMethod::Tools(ToolsMethod::List),
            params: None,
        }
    }

    /// Empty request `Parts` — for tests that don't care about inbound headers.
    fn empty_request_parts() -> RequestParts {
        axum::http::Request::builder()
            .method("POST")
            .uri("/")
            .body(())
            .unwrap()
            .into_parts()
            .0
    }

    /// Build a single-MCP `Request` with empty inbound parts.
    fn mcp_req(id: i64) -> Request {
        Request::Mcp(empty_request_parts(), mcp_request(id))
    }

    /// Build a batch `Request` with empty inbound parts.
    fn mcp_batch(ids: &[i64]) -> Request {
        Request::McpBatch(
            empty_request_parts(),
            ids.iter().copied().map(mcp_request).collect(),
        )
    }

    /// Tests that expect a buffered response unwrap `RouterOutput::Single`.
    /// Streaming variant tests use a separate helper to drain the stream.
    fn expect_single(output: RouterOutput) -> Response {
        match output {
            RouterOutput::Single(r) => r,
            RouterOutput::Stream(..) => panic!("expected Single, got Stream"),
        }
    }

    async fn drain_stream(output: RouterOutput) -> (ResponseParts, Vec<Response>) {
        let RouterOutput::Stream(parts, stream) = output else {
            panic!("expected Stream");
        };
        let mut out = Vec::new();
        futures_util::pin_mut!(stream);
        while let Some(item) = stream.next().await {
            out.push(item.unwrap());
        }
        (parts, out)
    }

    /// Spawn an axum router on a random local port and return its base URL.
    /// The server task is dropped when the test completes — fine for tests.
    async fn spawn_upstream(router: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });
        format!("http://{addr}")
    }

    /// Echo handler — returns a JSON-RPC `Response` with the request's id
    /// and `result: {"echoed": true}`.
    async fn echo_jsonrpc(body: AxumBytes) -> axum::Json<Value> {
        let req: Value = serde_json::from_slice(&body).unwrap();
        let id = req.get("id").cloned().unwrap_or(Value::Null);
        axum::Json(json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {"echoed": true},
        }))
    }

    // ── MCP requests ──────────────────────────────────────────

    #[tokio::test]
    async fn process__mcp_request_forwards_to_upstream_and_returns_response() {
        let url = spawn_upstream(Router::new().route("/", post(echo_jsonrpc))).await;
        let stage = RouterStage::new(config_for(&url)).unwrap();

        let resp = expect_single(stage.process(mcp_req(42), state()).await.unwrap());

        let Response::Mcp(_, JsonRpcResult::Response(r)) = resp else {
            panic!("expected Response::Mcp(JsonRpcResult::Response)");
        };
        assert_eq!(r.id, RequestId::Number(42));
        assert_eq!(r.result.unwrap(), json!({"echoed": true}));
    }

    #[tokio::test]
    async fn process__mcp_batch_returns_results_in_request_order() {
        let url = spawn_upstream(Router::new().route("/", post(echo_jsonrpc))).await;
        let stage = RouterStage::new(config_for(&url)).unwrap();

        let resp = expect_single(stage.process(mcp_batch(&[1, 2, 3]), state()).await.unwrap());

        let Response::McpBatch(_, items) = resp else {
            panic!("expected Response::McpBatch");
        };
        assert_eq!(items.len(), 3);
        for (i, item) in items.iter().enumerate() {
            let JsonRpcResult::Response(r) = item else {
                panic!("expected JsonRpcResult::Response at index {i}");
            };
            assert_eq!(r.id, RequestId::Number((i + 1) as i64));
        }
    }

    #[tokio::test]
    async fn process__mcp_request_propagates_jsonrpc_error_response() {
        // Upstream returns a JSON-RPC error envelope `{jsonrpc, id, error}`,
        // which is the spec-compliant shape (per JSON-RPC 2.0 §5.1). The
        // `id` may be Null when the upstream couldn't determine it.
        async fn err_handler() -> axum::Json<Value> {
            axum::Json(json!({
                "jsonrpc": "2.0",
                "id": null,
                "error": {"code": -32603, "message": "boom"},
            }))
        }
        let url = spawn_upstream(Router::new().route("/", post(err_handler))).await;
        let stage = RouterStage::new(config_for(&url)).unwrap();

        let resp = expect_single(stage.process(mcp_req(1), state()).await.unwrap());

        let Response::Mcp(_, JsonRpcResult::Error(e)) = resp else {
            panic!("expected JsonRpcResult::Error");
        };
        assert_eq!(e.id, RequestId::Null);
        assert_eq!(e.error.code, -32603);
        assert_eq!(e.error.message, "boom");
    }

    #[tokio::test]
    async fn process__mcp_request_errors_when_upstream_returns_non_jsonrpc_body() {
        async fn html_handler() -> impl IntoResponse {
            (
                [(axum::http::header::CONTENT_TYPE, "text/html")],
                "<html></html>",
            )
        }
        let url = spawn_upstream(Router::new().route("/", post(html_handler))).await;
        let stage = RouterStage::new(config_for(&url)).unwrap();

        let err = match stage.process(mcp_req(1), state()).await {
            Ok(_) => panic!("expected error from non-JSON-RPC upstream body"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("non-JSON-RPC"));
    }

    #[tokio::test]
    async fn process__mcp_request_forwards_inbound_headers_to_upstream() {
        let captured: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let sink = captured.clone();
        let app = Router::new().route(
            "/",
            post(move |headers: HeaderMap, body: AxumBytes| {
                let sink = sink.clone();
                async move {
                    *sink.lock().unwrap() = headers
                        .get("mcp-session-id")
                        .map(|v| v.to_str().unwrap().to_string());
                    let req: Value = serde_json::from_slice(&body).unwrap();
                    axum::Json(json!({
                        "jsonrpc": "2.0",
                        "id": req.get("id").cloned().unwrap_or(Value::Null),
                        "result": {"ok": true},
                    }))
                }
            }),
        );
        let url = spawn_upstream(app).await;
        let stage = RouterStage::new(config_for(&url)).unwrap();

        let parts = axum::http::Request::builder()
            .method("POST")
            .uri("/")
            .header("mcp-session-id", "sess-xyz")
            .body(())
            .unwrap()
            .into_parts()
            .0;
        let req = Request::Mcp(parts, mcp_request(1));
        let _ = stage.process(req, state()).await.unwrap();

        assert_eq!(captured.lock().unwrap().as_deref(), Some("sess-xyz"));
    }

    #[tokio::test]
    async fn process__mcp_response_carries_upstream_headers() {
        let app = Router::new().route(
            "/",
            post(|body: AxumBytes| async move {
                let req: Value = serde_json::from_slice(&body).unwrap();
                let body = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": req.get("id").cloned().unwrap_or(Value::Null),
                    "result": {"ok": true},
                });
                (
                    [
                        (
                            axum::http::header::CONTENT_TYPE,
                            axum::http::HeaderValue::from_static("application/json"),
                        ),
                        (
                            axum::http::HeaderName::from_static("mcp-session-id"),
                            axum::http::HeaderValue::from_static("sess-xyz"),
                        ),
                    ],
                    axum::Json(body),
                )
                    .into_response()
            }),
        );
        let url = spawn_upstream(app).await;
        let stage = RouterStage::new(config_for(&url)).unwrap();

        let resp = expect_single(stage.process(mcp_req(1), state()).await.unwrap());
        let Response::Mcp(parts, _) = resp else {
            panic!("expected Mcp");
        };
        assert_eq!(parts.headers["mcp-session-id"], "sess-xyz");
    }

    // ── HTTP requests ─────────────────────────────────────────

    #[tokio::test]
    async fn process__http_request_returns_http_response() {
        let url =
            spawn_upstream(Router::new().route("/", post(|| async { "hello upstream" }))).await;
        let stage = RouterStage::new(config_for(&url)).unwrap();

        let req = Request::Http(
            axum::http::Request::post("/")
                .header("content-type", "text/plain")
                .body(Bytes::from_static(b"client body"))
                .unwrap(),
        );
        let resp = expect_single(stage.process(req, state()).await.unwrap());

        let Response::Http(http) = resp else {
            panic!("expected Response::Http");
        };
        assert_eq!(http.status(), 200);
        assert_eq!(http.body().as_ref(), b"hello upstream");
    }

    #[tokio::test]
    async fn process__http_request_forwards_authorization_header() {
        let captured: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let sink = captured.clone();
        let app = Router::new().route(
            "/",
            post(move |headers: HeaderMap| {
                let sink = sink.clone();
                async move {
                    *sink.lock().unwrap() = headers
                        .get("authorization")
                        .map(|v| v.to_str().unwrap().to_string());
                    "ok"
                }
            }),
        );
        let url = spawn_upstream(app).await;
        let stage = RouterStage::new(config_for(&url)).unwrap();

        let req = Request::Http(
            axum::http::Request::post("/")
                .header("authorization", "Bearer xyz")
                .body(Bytes::new())
                .unwrap(),
        );
        let _ = stage.process(req, state()).await.unwrap();

        assert_eq!(captured.lock().unwrap().as_deref(), Some("Bearer xyz"));
    }

    // ── classify_upstream ─────────────────────────────────────

    fn rpc_result_str(id: i64) -> String {
        format!(r#"{{"jsonrpc":"2.0","id":{id},"result":{{"ok":true}}}}"#)
    }

    fn buffered_resp(
        content_type: &str,
        body: &str,
    ) -> hyper::Response<http_body_util::Full<Bytes>> {
        hyper::Response::builder()
            .status(200)
            .header("content-type", content_type)
            .body(http_body_util::Full::new(Bytes::copy_from_slice(
                body.as_bytes(),
            )))
            .unwrap()
    }

    #[tokio::test]
    async fn classify_upstream__json_returns_single_mcp() {
        let resp = buffered_resp("application/json", &rpc_result_str(1));
        let output = classify_upstream(resp).await.unwrap();
        let resp = expect_single(output);
        assert!(matches!(resp, Response::Mcp(_, JsonRpcResult::Response(_))));
    }

    #[tokio::test]
    async fn classify_upstream__sse_returns_stream() {
        let body = format!("event: message\ndata: {}\n\n", rpc_result_str(1));
        let resp = buffered_resp("text/event-stream", &body);
        let output = classify_upstream(resp).await.unwrap();
        assert!(matches!(output, RouterOutput::Stream(..)));
    }

    #[tokio::test]
    async fn classify_upstream__sse_stream_yields_each_message_frame() {
        let body = format!(
            "data: {}\n\ndata: {}\n\n",
            rpc_result_str(1),
            rpc_result_str(2)
        );
        let resp = buffered_resp("text/event-stream", &body);
        let (_, items) = drain_stream(classify_upstream(resp).await.unwrap()).await;
        assert_eq!(items.len(), 2);
        for item in &items {
            assert!(matches!(item, Response::Mcp(_, _)));
        }
    }

    #[tokio::test]
    async fn classify_upstream__sse_drops_non_message_and_unparseable_frames() {
        let body = format!(
            "event: notifications/progress\ndata: {{\"x\":1}}\n\ndata: not-json\n\nevent: message\ndata: {}\n\n",
            rpc_result_str(7)
        );
        let resp = buffered_resp("text/event-stream", &body);
        let (_, items) = drain_stream(classify_upstream(resp).await.unwrap()).await;
        assert_eq!(items.len(), 1);
    }

    #[tokio::test]
    async fn classify_upstream__sse_preserves_upstream_headers_on_stream_items() {
        let body = format!("data: {}\n\n", rpc_result_str(1));
        let resp = hyper::Response::builder()
            .status(200)
            .header("content-type", "text/event-stream")
            .header("mcp-session-id", "sess-xyz")
            .body(http_body_util::Full::new(Bytes::copy_from_slice(
                body.as_bytes(),
            )))
            .unwrap();
        let (_, items) = drain_stream(classify_upstream(resp).await.unwrap()).await;
        assert_eq!(items.len(), 1);
        let Response::Mcp(parts, _) = &items[0] else {
            panic!("expected Mcp");
        };
        assert_eq!(parts.headers["mcp-session-id"], "sess-xyz");
    }

    #[tokio::test]
    async fn classify_upstream__sse_carries_envelope_parts_on_router_output() {
        // Regression: encode_stream must see upstream `mcp-session-id`
        // before the first frame, otherwise session negotiation breaks
        // (Inspector observed "No valid session ID" loop).
        let body = format!("data: {}\n\n", rpc_result_str(1));
        let resp = hyper::Response::builder()
            .status(200)
            .header("content-type", "text/event-stream")
            .header("mcp-session-id", "sess-xyz")
            .header("set-cookie", "k=v; Path=/")
            .body(http_body_util::Full::new(Bytes::copy_from_slice(
                body.as_bytes(),
            )))
            .unwrap();
        let (parts, _) = drain_stream(classify_upstream(resp).await.unwrap()).await;
        assert_eq!(parts.headers["mcp-session-id"], "sess-xyz");
        assert_eq!(parts.headers["set-cookie"], "k=v; Path=/");
    }

    // ── GET SSE ───────────────────────────────────────────────

    fn get_request_parts(headers: &[(&str, &str)]) -> RequestParts {
        let mut builder = axum::http::Request::builder().method("GET").uri("/");
        for (k, v) in headers {
            builder = builder.header(*k, *v);
        }
        builder.body(()).unwrap().into_parts().0
    }

    async fn drain_axum_body(resp: AxumResponse) -> String {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }

    /// Two server-initiated notifications — neither parses as
    /// `JsonRpcResult`. The whole point of GET SSE is to forward these,
    /// so the test body uses them instead of result-shaped frames.
    async fn spawn_get_sse_notifications() -> String {
        async fn handler() -> AxumResponse {
            let body = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/tools/list_changed\"}\n\nevent: message\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/message\",\"params\":{\"level\":\"info\"}}\n\n";
            AxumResponse::builder()
                .status(200)
                .header("content-type", "text/event-stream")
                .body(axum::body::Body::from(body))
                .unwrap()
        }
        spawn_upstream(Router::new().route("/", axum_get(handler))).await
    }

    #[tokio::test]
    async fn open_get_sse__forwards_notifications_unmodified() {
        let url = spawn_get_sse_notifications().await;
        let stage = RouterStage::new(config_for(&url)).unwrap();

        let resp = stage
            .open_get_sse(get_request_parts(&[("mcp-session-id", "sess-1")]))
            .await
            .unwrap();
        assert_eq!(resp.headers()["content-type"], "text/event-stream");

        let body = drain_axum_body(resp).await;
        assert!(body.contains("notifications/tools/list_changed"));
        assert!(body.contains("notifications/message"));
        assert_eq!(body.matches("event: message").count(), 2);
    }

    #[tokio::test]
    async fn open_get_sse__forwards_session_and_last_event_id_headers() {
        async fn handler(
            AxumState(captured): AxumState<Arc<Mutex<HeaderMap>>>,
            headers: HeaderMap,
        ) -> AxumResponse {
            *captured.lock().unwrap() = headers;
            AxumResponse::builder()
                .status(200)
                .header("content-type", "text/event-stream")
                .body(axum::body::Body::from("event: message\ndata: {}\n\n"))
                .unwrap()
        }
        let captured = Arc::new(Mutex::new(HeaderMap::new()));
        let app = Router::new()
            .route("/", axum_get(handler))
            .with_state(captured.clone());
        let url = spawn_upstream(app).await;
        let stage = RouterStage::new(config_for(&url)).unwrap();

        stage
            .open_get_sse(get_request_parts(&[
                ("mcp-session-id", "sess-xyz"),
                ("last-event-id", "42"),
                ("authorization", "Bearer abc"),
            ]))
            .await
            .unwrap();

        let h = captured.lock().unwrap();
        assert_eq!(h["mcp-session-id"], "sess-xyz");
        assert_eq!(h["last-event-id"], "42");
        assert_eq!(h["authorization"], "Bearer abc");
        assert_eq!(h["accept"], "text/event-stream");
    }

    #[tokio::test]
    async fn open_get_sse__preserves_upstream_status_and_headers() {
        async fn handler() -> AxumResponse {
            AxumResponse::builder()
                .status(200)
                .header("content-type", "text/event-stream")
                .header("mcp-session-id", "sess-xyz")
                .header("set-cookie", "k=v; Path=/")
                .body(axum::body::Body::from("event: message\ndata: {}\n\n"))
                .unwrap()
        }
        let url = spawn_upstream(Router::new().route("/", axum_get(handler))).await;
        let stage = RouterStage::new(config_for(&url)).unwrap();

        let resp = stage.open_get_sse(get_request_parts(&[])).await.unwrap();

        assert_eq!(resp.status(), 200);
        assert_eq!(resp.headers()["mcp-session-id"], "sess-xyz");
        assert_eq!(resp.headers()["set-cookie"], "k=v; Path=/");
        assert!(!resp.headers().contains_key("transfer-encoding"));
    }

    #[tokio::test]
    async fn open_get_sse__upstream_405_passes_through() {
        async fn handler() -> AxumResponse {
            AxumResponse::builder()
                .status(405)
                .body(axum::body::Body::from(""))
                .unwrap()
        }
        let url = spawn_upstream(Router::new().route("/", axum_get(handler))).await;
        let stage = RouterStage::new(config_for(&url)).unwrap();

        let resp = stage.open_get_sse(get_request_parts(&[])).await.unwrap();
        assert_eq!(resp.status(), 405);
    }

    /// Mirrors the real Inspector scenario: upstream responds with SSE
    /// headers immediately, then idles indefinitely waiting for events.
    /// The proxy must return the response (with headers) without waiting
    /// for the first body frame.
    #[tokio::test]
    async fn open_get_sse__returns_headers_before_first_frame() {
        async fn handler() -> AxumResponse {
            let stream = futures_util::stream::pending::<Result<bytes::Bytes, std::io::Error>>();
            AxumResponse::builder()
                .status(200)
                .header("content-type", "text/event-stream")
                .body(axum::body::Body::from_stream(stream))
                .unwrap()
        }
        let url = spawn_upstream(Router::new().route("/", axum_get(handler))).await;
        let stage = RouterStage::new(config_for(&url)).unwrap();

        let resp = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            stage.open_get_sse(get_request_parts(&[("mcp-session-id", "sess-1")])),
        )
        .await
        .expect("response headers should arrive without waiting on body")
        .unwrap();

        assert_eq!(resp.status(), 200);
        assert_eq!(resp.headers()["content-type"], "text/event-stream");
    }

    /// Hop-by-hop headers (e.g. `connection: close`) from upstream must
    /// not leak through — they cause clients to tear down the socket
    /// after the response head, which surfaces as ECONNRESET.
    #[tokio::test]
    async fn open_get_sse__strips_upstream_hop_by_hop_headers() {
        async fn handler() -> AxumResponse {
            AxumResponse::builder()
                .status(200)
                .header("content-type", "text/event-stream")
                .header("connection", "close")
                .header("keep-alive", "timeout=5")
                .body(axum::body::Body::from("event: message\ndata: {}\n\n"))
                .unwrap()
        }
        let url = spawn_upstream(Router::new().route("/", axum_get(handler))).await;
        let stage = RouterStage::new(config_for(&url)).unwrap();

        let resp = stage.open_get_sse(get_request_parts(&[])).await.unwrap();
        assert!(!resp.headers().contains_key("connection"));
        assert!(!resp.headers().contains_key("keep-alive"));
        assert_eq!(resp.headers()["content-type"], "text/event-stream");
    }
}
