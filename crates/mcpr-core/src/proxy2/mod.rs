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

use crate::protocol::{Request, Response};
use crate::proxy2::{
    proxy_config::ProxyConfig,
    stage::{StagePipeline, router_stage::RouterStage},
    state::InnerProxyState,
};

/// Build an axum app that runs a single proxy from `cfg`. Pipeline has no
/// pre/post stages yet — every request flows: axum → `Request::from_axum`
/// → `StagePipeline::process` → axum response.
pub fn build_app(cfg: Arc<ProxyConfig>) -> anyhow::Result<Router> {
    let router_stage = RouterStage::new(cfg)?;
    let pipeline = Arc::new(StagePipeline::new(
        vec![],
        vec![],
        router_stage,
        Arc::new(InnerProxyState {}),
    ));

    Ok(Router::new()
        .fallback(any(handle_request))
        .with_state(pipeline))
}

async fn handle_request(
    State(pipeline): State<Arc<StagePipeline>>,
    req: AxumRequest,
) -> AxumResponse {
    let parsed = match Request::from_axum(req).await {
        Ok(r) => r,
        Err(e) => {
            return (axum::http::StatusCode::BAD_REQUEST, format!("parse: {e}")).into_response();
        }
    };
    match pipeline.process(parsed).await {
        Ok(resp) => to_axum_response(resp),
        Err(e) => (
            axum::http::StatusCode::BAD_GATEWAY,
            format!("upstream: {e}"),
        )
            .into_response(),
    }
}

fn to_axum_response(resp: Response) -> AxumResponse {
    match resp {
        Response::Mcp(result) => axum::Json(result).into_response(),
        Response::McpBatch(results) => axum::Json(results).into_response(),
        Response::Http(http) => {
            let (parts, bytes) = http.into_parts();
            AxumResponse::from_parts(parts, axum::body::Body::from(bytes))
        }
    }
}
