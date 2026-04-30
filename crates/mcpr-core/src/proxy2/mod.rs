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
    event::EventBus,
    protocol::{Request, Response},
    proxy2::stage::session_stage::SessionStage,
};
use crate::{
    protocol::session::SessionStore,
    proxy2::{
        proxy_config::ProxyConfig,
        stage::{
            StagePipeline,
            csp_rewritten_stage::{CspRewriteConfig, CspRewritter},
            log_stage::{RequestLogStage, ResponseLogStage},
            router_stage::RouterStage,
            schema_tracking_stage::SchemaTrackingStage,
            types::{RequestStage, ResponseStage},
        },
        state::InnerProxyState,
    },
};

/// Build an axum app that runs a single proxy from `cfg`. Every request
/// flows: axum → `Request::from_axum` → `StagePipeline::process` → axum
/// response. The CSP rewrite stage runs after the router so widget metas
/// in upstream responses are mutated before they reach the client.
pub fn build_app(cfg: Arc<ProxyConfig>, event_bus: EventBus) -> anyhow::Result<Router> {
    let cors = CorsLayer::permissive();

    // Build Stages
    let csp_rewritter = CspRewritter::new(CspRewriteConfig::from_proxy_config(&cfg));
    let router_stage = RouterStage::new(cfg)?;
    let request_stages: Vec<Box<dyn RequestStage>> =
        vec![Box::new(RequestLogStage), Box::new(SessionStage)];
    let response_stages: Vec<Box<dyn ResponseStage>> = vec![
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
        Response::Mcp(parts, result) => mcp_axum_response(parts, axum::Json(result)),
        Response::McpBatch(parts, results) => mcp_axum_response(parts, axum::Json(results)),
        Response::Http(http) => {
            let (parts, bytes) = http.into_parts();
            AxumResponse::from_parts(parts, axum::body::Body::from(bytes))
        }
    }
}

/// Build an axum response that re-uses the upstream `parts` (status + headers)
/// but replaces the body with our re-serialized JSON. We strip headers the
/// HTTP layer recalculates (`content-length`) and force `content-type` so the
/// body byte count and media type match what we actually wrote.
fn mcp_axum_response<B: IntoResponse>(
    mut parts: axum::http::response::Parts,
    body: B,
) -> AxumResponse {
    parts.headers.remove(axum::http::header::CONTENT_LENGTH);
    parts.headers.remove(axum::http::header::TRANSFER_ENCODING);
    parts.headers.insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/json"),
    );
    let mut resp = body.into_response();
    *resp.status_mut() = parts.status;
    let resp_headers = resp.headers_mut();
    for (name, value) in parts.headers.iter() {
        resp_headers.insert(name, value.clone());
    }
    resp
}
