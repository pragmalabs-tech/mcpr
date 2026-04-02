use std::time::Instant;

use axum::{
    Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, Method},
    response::Response,
    routing::any,
};

use crate::AppState;
use crate::logger::LogEntry;
use crate::mcp_handler::{handle_mcp_post, handle_mcp_sse};
use crate::passthrough::{forward_and_passthrough, serve_oauth_callback_relay};
use crate::widgets::{list_widgets, serve_widget_asset, serve_widget_html};
use mcpr_core::router::{ClassifiedRequest, classify};
use mcpr_core::sse::split_upstream;
use mcpr_events::{EventType, McprEvent};
use mcpr_session::SessionStore;

/// Convenience wrapper: forward a request using this app's state.
pub(crate) async fn forward_request(
    state: &AppState,
    url: &str,
    method: Method,
    headers: &HeaderMap,
    body: &Bytes,
    is_streaming: bool,
) -> Result<reqwest::Response, reqwest::Error> {
    mcpr_core::forwarding::forward_request(
        &state.upstream,
        url,
        method,
        headers,
        body,
        is_streaming,
    )
    .await
}

/// All proxy routes — catch-all that routes by method + content-type.
pub fn proxy_routes(router: Router<AppState>) -> Router<AppState> {
    router.fallback(any(handle_request))
}

/// Catch-all handler: classify the request, then dispatch to the appropriate handler.
async fn handle_request(
    State(state): State<AppState>,
    method: Method,
    headers: HeaderMap,
    uri: axum::http::Uri,
    body: Bytes,
) -> Response {
    let start = Instant::now();
    let path = uri.path();
    let has_widgets = state.widget_source.is_some();

    match classify(&method, path, &headers, &body, has_widgets) {
        ClassifiedRequest::OAuthCallback => serve_oauth_callback_relay().await,

        ClassifiedRequest::WidgetHtml { name } => serve_widget_html(&state, &name).await,

        ClassifiedRequest::WidgetList => list_widgets(&state).await,

        ClassifiedRequest::WidgetAsset => serve_widget_asset(&state, path).await,

        ClassifiedRequest::McpPost { parsed } => {
            handle_mcp_post(&state, path, &headers, &body, parsed, start).await
        }

        ClassifiedRequest::McpSse => handle_mcp_sse(&state, path, &headers, start).await,

        ClassifiedRequest::Passthrough => {
            // DELETE session cleanup (pre-processing before passthrough)
            if method == Method::DELETE
                && let Some(sid) = headers.get("mcp-session-id").and_then(|v| v.to_str().ok())
            {
                state
                    .logger
                    .emit(LogEntry::new("DELETE", path, 0, "session:closed").session_id(sid));
                state
                    .events
                    .emit(McprEvent::new(EventType::SessionEnd).session(sid));
                state.sessions.remove(sid).await;
            }

            let (base, _) = split_upstream(&state.mcp_upstream);
            let upstream_url = format!("{}{}", base.trim_end_matches('/'), path);
            forward_and_passthrough(&state, &upstream_url, method, path, &headers, &body, start)
                .await
        }
    }
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
    ) -> crate::AppState {
        use std::sync::Arc;
        use tokio::sync::RwLock;
        crate::AppState {
            mcp_upstream: upstream_url.to_string(),
            widget_source: None,
            rewrite_config: Arc::new(RwLock::new(mcpr_widgets::RewriteConfig {
                proxy_url: "http://localhost:0".to_string(),
                proxy_domain: "localhost".to_string(),
                mcp_upstream: upstream_url.to_string(),
                extra_csp_domains: vec![],
                csp_mode: mcpr_widgets::CspMode::default(),
            })),
            upstream: mcpr_core::forwarding::UpstreamClient {
                http_client: reqwest::Client::builder()
                    .connect_timeout(std::time::Duration::from_secs(5))
                    .build()
                    .unwrap(),
                semaphore: Arc::new(tokio::sync::Semaphore::new(100)),
                request_timeout: std::time::Duration::from_secs(30),
            },
            tui_state: crate::tui::new_shared_state(),
            logger: crate::logger::LogRouter::start(vec![]).router,
            events: Arc::new(mcpr_events::NoopEmitter),
            sessions: mcpr_session::MemorySessionStore::new(),
            max_request_body: max_request,
            max_response_body: max_response,
        }
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
