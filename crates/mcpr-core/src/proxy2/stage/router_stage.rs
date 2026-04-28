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
