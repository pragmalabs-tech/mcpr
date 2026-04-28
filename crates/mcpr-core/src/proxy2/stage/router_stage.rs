//! Terminal stage: forwards `Request` to upstream via the sharded
//! HTTP/2 client pool and turns the upstream answer into `Response`.
//! MCP requests are sharded by JSON-RPC id; raw HTTP requests pick a
//! random shard since they have no session identity.

use std::convert::Infallible;
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{BodyExt, Full, combinators::BoxBody};
use hyper::Method;

use crate::{
    protocol::{
        Request, Response,
        http_request::HttpRequest,
        mcp::{JsonRpcRequest, JsonRpcResult},
    },
    proxy2::{proxy_config::ProxyConfig, state::ProxyState, upstream::pool::UpstreamPool},
};

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
    pub async fn process(&self, request: Request, state: ProxyState) -> anyhow::Result<Response> {
        match request {
            Request::Mcp(req) => handle_mcp_request(req, &state, &self.pool).await,
            Request::McpBatch(reqs) => handle_mcp_requests(reqs, &state, &self.pool).await,
            Request::Http(req) => handle_http_request(req, &state, &self.pool).await,
        }
    }
}

async fn handle_mcp_requests(
    requests: Vec<JsonRpcRequest>,
    state: &ProxyState,
    pool: &UpstreamPool,
) -> anyhow::Result<Response> {
    let mut res = vec![];

    for request in requests {
        res.push(process_mcp_request(request, state, pool).await?);
    }

    Ok(Response::McpBatch(res))
}

async fn handle_mcp_request(
    request: JsonRpcRequest,
    state: &ProxyState,
    pool: &UpstreamPool,
) -> anyhow::Result<Response> {
    Ok(Response::Mcp(
        process_mcp_request(request, state, pool).await?,
    ))
}

async fn process_mcp_request(
    request: JsonRpcRequest,
    _state: &ProxyState,
    pool: &UpstreamPool,
) -> anyhow::Result<JsonRpcResult> {
    let body = Bytes::from(serde_json::to_vec(&request)?);

    let upstream_req = hyper::Request::builder()
        .method(Method::POST)
        .uri(pool.upstream().url.as_str())
        .header(hyper::header::CONTENT_TYPE, "application/json")
        .header(hyper::header::ACCEPT, "application/json")
        .body(box_full(body))?;

    let resp = pool.pick(&request.id).request(upstream_req).await?;

    match Response::from_hyper(resp).await? {
        Response::Mcp(result) => Ok(result),
        Response::McpBatch(_) => Err(anyhow::anyhow!(
            "upstream returned a JSON-RPC batch for a single MCP request"
        )),
        Response::Http(http) => Err(anyhow::anyhow!(
            "upstream returned non-JSON-RPC body (status {})",
            http.status()
        )),
    }
}

async fn handle_http_request(
    request: HttpRequest,
    _state: &ProxyState,
    pool: &UpstreamPool,
) -> anyhow::Result<Response> {
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
    Ok(Response::from_hyper(resp).await?)
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
        csp::CspConfig,
        protocol::mcp::{
            ClientMethod, JsonRpcRequest, JsonRpcResult, JsonRpcVersion, RequestId, ToolsMethod,
        },
        proxy2::state::InnerProxyState,
    };
    use axum::{
        Router, body::Bytes as AxumBytes, http::HeaderMap, response::IntoResponse, routing::post,
    };
    use serde_json::{Value, json};
    use std::sync::{Arc, Mutex};

    // ── Helpers ───────────────────────────────────────────────

    fn config_for(url: &str) -> Arc<ProxyConfig> {
        Arc::new(ProxyConfig {
            name: "test".into(),
            mcp: url.to_string(),
            port: None,
            csp: CspConfig::default(),
            max_request_body_size: None,
            max_response_body_size: None,
            max_concurrent_upstream: None,
            connect_timeout: None,
            request_timeout: None,
        })
    }

    fn state() -> ProxyState {
        Arc::new(InnerProxyState {})
    }

    fn mcp_request(id: i64) -> JsonRpcRequest {
        JsonRpcRequest {
            jsonrpc: JsonRpcVersion,
            id: RequestId::Number(id),
            method: ClientMethod::Tools(ToolsMethod::List),
            params: None,
        }
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

        let resp = stage
            .process(Request::Mcp(mcp_request(42)), state())
            .await
            .unwrap();

        let Response::Mcp(JsonRpcResult::Response(r)) = resp else {
            panic!("expected Response::Mcp(JsonRpcResult::Response)");
        };
        assert_eq!(r.id, RequestId::Number(42));
        assert_eq!(r.result.unwrap(), json!({"echoed": true}));
    }

    #[tokio::test]
    async fn process__mcp_batch_returns_results_in_request_order() {
        let url = spawn_upstream(Router::new().route("/", post(echo_jsonrpc))).await;
        let stage = RouterStage::new(config_for(&url)).unwrap();

        let batch = Request::McpBatch(vec![mcp_request(1), mcp_request(2), mcp_request(3)]);
        let resp = stage.process(batch, state()).await.unwrap();

        let Response::McpBatch(items) = resp else {
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
        async fn err_handler() -> axum::Json<Value> {
            axum::Json(json!({"code": -32603, "message": "boom"}))
        }
        let url = spawn_upstream(Router::new().route("/", post(err_handler))).await;
        let stage = RouterStage::new(config_for(&url)).unwrap();

        let resp = stage
            .process(Request::Mcp(mcp_request(1)), state())
            .await
            .unwrap();

        let Response::Mcp(JsonRpcResult::Error(e)) = resp else {
            panic!("expected JsonRpcResult::Error");
        };
        assert_eq!(e.code, -32603);
        assert_eq!(e.message, "boom");
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

        let err = match stage.process(Request::Mcp(mcp_request(1)), state()).await {
            Ok(_) => panic!("expected error from non-JSON-RPC upstream body"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("non-JSON-RPC"));
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
        let resp = stage.process(req, state()).await.unwrap();

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
}
