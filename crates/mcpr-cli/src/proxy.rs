//! Axum glue — wraps the per-request pipeline in a catch-all fallback route.

use std::time::Instant;

use axum::{
    Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, Method},
    response::{IntoResponse, Response},
    routing::any,
};

use crate::state::AppState;
use mcpr_core::proxy::emit;
use mcpr_core::proxy::intake::from_axum_parts;
use mcpr_core::proxy::pipeline::driver::StageGuard;
use mcpr_core::proxy::pipeline::values::{Context, Intake, Working};

/// All proxy routes — catch-all that routes by method + content-type.
pub fn proxy_routes(router: Router<AppState>) -> Router<AppState> {
    router.fallback(any(handle_request))
}

/// Catch-all axum handler — drives one request through the full target
/// pipeline: content-based intake → request middleware chain → router →
/// transport → response middleware chain → emit → axum response.
async fn handle_request(
    State(state): State<AppState>,
    method: Method,
    headers: HeaderMap,
    uri: axum::http::Uri,
    body: Bytes,
) -> Response {
    let start = Instant::now();
    let request_size = body.len();
    let path = uri.path().to_string();
    let http_method = method.clone();

    let mut cx = Context {
        intake: Intake {
            start,
            proxy: state.proxy.clone(),
            http_method,
            path,
            request_size,
        },
        working: Working::default(),
    };
    // Scoped guard pushes `intake_parse` timing on drop (skipped when
    // `MCPR_STAGE_TIMING` is off).
    let req = {
        let _g = StageGuard::start("intake_parse", &mut cx.working.timings);
        from_axum_parts(method, headers, uri, body)
    };

    let resp = state.pipeline.run(req, &mut cx).await;
    emit::emit(&cx, &resp);
    resp.into_response()
}

#[cfg(test)]
mod tests {
    use axum::{
        Router,
        body::Bytes,
        http::{StatusCode, header},
    };

    fn test_app_state_with_limit(
        upstream_url: &str,
        max_request: usize,
        max_response: usize,
    ) -> crate::state::AppState {
        use std::sync::Arc;
        let proxy = Arc::new(mcpr_core::proxy::ProxyState {
            name: "test".to_string(),
            mcp_upstream: upstream_url.to_string(),
            upstream: mcpr_core::proxy::forwarding::UpstreamClient {
                http_client: reqwest::Client::builder()
                    .connect_timeout(std::time::Duration::from_secs(5))
                    .build()
                    .unwrap(),
                request_timeout: std::time::Duration::from_secs(30),
            },
            max_request_body: max_request,
            max_response_body: max_response,
            max_concurrent_upstream: 100,
            rewrite_config: mcpr_core::proxy::RewriteConfig {
                proxy_url: "http://localhost:0".to_string(),
                proxy_domain: "localhost".to_string(),
                mcp_upstream: upstream_url.to_string(),
                csp: mcpr_core::proxy::CspConfig::default(),
            }
            .into_swap(),
            sessions: mcpr_core::protocol::session::SessionStore::new(),
            health: mcpr_core::proxy::new_shared_health(),
            event_bus: mcpr_core::event::EventManager::new().start().bus,
        });
        let pipeline = std::sync::Arc::new(mcpr_core::proxy::build_default_pipeline(
            proxy.rewrite_config.clone(),
        ));
        crate::state::AppState { proxy, pipeline }
    }

    #[tokio::test]
    async fn body_limit_rejects_oversized_request() {
        use axum::routing::post;

        let upstream = Router::new().route("/mcp", post(|body: Bytes| async move { body }));
        let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_addr = upstream_listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(upstream_listener, upstream).await.unwrap() });

        let upstream_url = format!("http://{upstream_addr}/mcp");
        let state = test_app_state_with_limit(&upstream_url, 1024, 10 * 1024 * 1024);
        let app = crate::build_app(state);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let client = reqwest::Client::new();
        let url = format!("http://{addr}/mcp");

        let small_body = br#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
        let resp = client
            .post(&url)
            .header("content-type", "application/json")
            .body(small_body.to_vec())
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);

        let large_body = vec![b'x'; 2048];
        let resp = client.post(&url).body(large_body).send().await.unwrap();
        assert_eq!(resp.status(), 413);
    }

    #[tokio::test]
    async fn response_body_cap_rejects_oversized_upstream() {
        use axum::routing::post;

        let upstream = Router::new().route(
            "/mcp",
            post(|| async {
                let big = vec![b'A'; 2048];
                (
                    StatusCode::OK,
                    [(header::CONTENT_TYPE, "application/json")],
                    big,
                )
            }),
        );
        let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_addr = upstream_listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(upstream_listener, upstream).await.unwrap() });

        let upstream_url = format!("http://{upstream_addr}/mcp");
        let state = test_app_state_with_limit(&upstream_url, 5 * 1024 * 1024, 1024);
        let app = crate::build_app(state);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let client = reqwest::Client::new();
        let url = format!("http://{addr}/mcp");

        let body = br#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
        let resp = client
            .post(&url)
            .header("content-type", "application/json")
            .body(body.to_vec())
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            502,
            "oversized upstream response should be rejected"
        );
    }
}
