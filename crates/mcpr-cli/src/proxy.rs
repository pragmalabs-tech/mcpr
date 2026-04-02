use std::time::Instant;

use axum::{
    Router,
    body::{Body, Bytes},
    extract::State,
    http::{HeaderMap, Method, StatusCode, header},
    response::{IntoResponse, Response},
    routing::any,
};
use futures_util::StreamExt;

use crate::AppState;
use crate::logger::LogEntry;
use crate::mcp_handler::{handle_mcp_post, handle_mcp_sse};
use crate::passthrough::{forward_and_passthrough, serve_oauth_callback_relay};
use crate::router::{ClassifiedRequest, classify};
use crate::widgets::{list_widgets, serve_widget_asset, serve_widget_html};
use mcpr_session::SessionStore;

// ── Shared utilities (used by mcp_handler, passthrough) ──

/// Read a response body with a size cap. Returns 502 if the upstream response exceeds `max_bytes`.
pub(crate) async fn read_body_capped(
    resp: reqwest::Response,
    max_bytes: usize,
) -> Result<Bytes, Response> {
    if let Some(len) = resp.content_length()
        && len as usize > max_bytes
    {
        return Err((StatusCode::BAD_GATEWAY, "upstream response too large").into_response());
    }

    let mut body =
        Vec::with_capacity(resp.content_length().unwrap_or(0).min(max_bytes as u64) as usize);
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| {
            (StatusCode::BAD_GATEWAY, format!("upstream read error: {e}")).into_response()
        })?;
        if body.len() + chunk.len() > max_bytes {
            return Err((StatusCode::BAD_GATEWAY, "upstream response too large").into_response());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(Bytes::from(body))
}

/// Check if the response body is SSE-formatted and extract the JSON data.
/// Returns the extracted JSON bytes if exactly one `data:` event is found.
pub(crate) fn extract_json_from_sse(bytes: &[u8]) -> Option<Vec<u8>> {
    let text = std::str::from_utf8(bytes).ok()?;
    if !text.trim_start().starts_with("data:") && !text.contains("\ndata:") {
        return None;
    }
    let mut json_parts = Vec::new();
    for line in text.lines() {
        if let Some(data) = line.strip_prefix("data:") {
            let data = data.trim_start();
            if !data.is_empty() {
                json_parts.push(data);
            }
        }
    }
    if json_parts.len() == 1 {
        Some(json_parts[0].as_bytes().to_vec())
    } else {
        None
    }
}

/// Re-wrap JSON bytes into SSE format.
pub(crate) fn wrap_as_sse(json_bytes: &[u8]) -> Vec<u8> {
    let mut out = b"data: ".to_vec();
    out.extend_from_slice(json_bytes);
    out.extend_from_slice(b"\n\n");
    out
}

/// Split a full upstream URL into (base, path).
/// e.g. "http://localhost:9000/mcp" → ("http://localhost:9000", "/mcp")
pub(crate) fn split_upstream(url: &str) -> (&str, &str) {
    let after_scheme = if let Some(pos) = url.find("://") {
        pos + 3
    } else {
        0
    };
    match url[after_scheme..].find('/') {
        Some(pos) => url.split_at(after_scheme + pos),
        None => (url, ""),
    }
}

/// Send a request to the upstream server, forwarding relevant headers.
/// When `is_streaming` is false, applies the configured request timeout.
pub(crate) async fn forward_request(
    state: &AppState,
    url: &str,
    method: Method,
    headers: &HeaderMap,
    body: &Bytes,
    is_streaming: bool,
) -> Result<reqwest::Response, reqwest::Error> {
    let _permit = state
        .upstream_semaphore
        .acquire()
        .await
        .expect("upstream semaphore closed");

    let mut req = state.http_client.request(method, url);

    if !is_streaming {
        req = req.timeout(state.request_timeout);
    }

    for key in [header::AUTHORIZATION, header::CONTENT_TYPE, header::ACCEPT] {
        if let Some(val) = headers.get(&key) {
            req = req.header(key.as_str(), val.as_bytes());
        }
    }

    if let Some(session_id) = headers.get("mcp-session-id") {
        req = req.header("mcp-session-id", session_id.as_bytes());
    }

    if let Some(last_event) = headers.get("last-event-id") {
        req = req.header("last-event-id", last_event.as_bytes());
    }

    if !body.is_empty() {
        req = req.body(body.clone());
    }

    req.send().await
}

/// Build an axum Response from status, headers, and body.
pub(crate) fn build_response(status: u16, upstream_headers: &HeaderMap, body: Body) -> Response {
    let status_code = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut builder = Response::builder().status(status_code);

    for key in [header::CONTENT_TYPE, header::CACHE_CONTROL] {
        if let Some(val) = upstream_headers.get(&key) {
            builder = builder.header(key.as_str(), val);
        }
    }

    if let Some(val) = upstream_headers.get("mcp-session-id") {
        builder = builder.header("mcp-session-id", val);
    }

    if let Some(val) = upstream_headers.get(header::WWW_AUTHENTICATE) {
        builder = builder.header(header::WWW_AUTHENTICATE, val);
    }

    builder.body(body).unwrap_or_else(|_| {
        Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .body(Body::from("Failed to build response"))
            .unwrap()
    })
}

// ── Routes + dispatcher ──

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
    use super::*;

    // ── SSE extraction ──

    #[test]
    fn extract_json_from_sse_single_event() {
        let input = b"data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n\n";
        let result = extract_json_from_sse(input).unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&result).unwrap();
        assert_eq!(parsed["jsonrpc"], "2.0");
    }

    #[test]
    fn extract_json_from_sse_with_leading_whitespace_returns_none() {
        let input = b"  data: {\"id\":1}\n\n";
        assert!(extract_json_from_sse(input).is_none());
    }

    #[test]
    fn extract_json_from_sse_not_sse() {
        let input = b"{\"jsonrpc\":\"2.0\",\"id\":1}";
        assert!(extract_json_from_sse(input).is_none());
    }

    #[test]
    fn extract_json_from_sse_multiple_events_returns_none() {
        let input = b"data: {\"id\":1}\n\ndata: {\"id\":2}\n\n";
        assert!(extract_json_from_sse(input).is_none());
    }

    #[test]
    fn extract_json_from_sse_empty_data_skipped() {
        let input = b"data: \ndata: {\"id\":1}\n\n";
        let result = extract_json_from_sse(input);
        assert!(result.is_some());
    }

    // ── SSE wrapping ──

    #[test]
    fn wrap_as_sse_format() {
        let json = b"{\"id\":1}";
        let wrapped = wrap_as_sse(json);
        assert_eq!(wrapped, b"data: {\"id\":1}\n\n");
    }

    #[test]
    fn sse_roundtrip() {
        let original = b"{\"jsonrpc\":\"2.0\",\"id\":42,\"result\":{\"content\":[]}}";
        let wrapped = wrap_as_sse(original);
        let extracted = extract_json_from_sse(&wrapped).unwrap();
        assert_eq!(extracted, original);
    }

    // ── split_upstream ──

    #[test]
    fn split_upstream_with_path() {
        let (base, path) = split_upstream("http://localhost:9000/mcp");
        assert_eq!(base, "http://localhost:9000");
        assert_eq!(path, "/mcp");
    }

    #[test]
    fn split_upstream_no_path() {
        let (base, path) = split_upstream("http://localhost:9000");
        assert_eq!(base, "http://localhost:9000");
        assert_eq!(path, "");
    }

    #[test]
    fn split_upstream_deep_path() {
        let (base, path) = split_upstream("https://api.example.com/v1/mcp");
        assert_eq!(base, "https://api.example.com");
        assert_eq!(path, "/v1/mcp");
    }

    #[test]
    fn split_upstream_trailing_slash() {
        let (base, path) = split_upstream("http://localhost:9000/");
        assert_eq!(base, "http://localhost:9000");
        assert_eq!(path, "/");
    }

    // ── DefaultBodyLimit integration ──

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
            http_client: reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(5))
                .build()
                .unwrap(),
            tui_state: crate::tui::new_shared_state(),
            logger: crate::logger::LogRouter::start(vec![]).router,
            sessions: mcpr_session::MemorySessionStore::new(),
            max_request_body: max_request,
            max_response_body: max_response,
            request_timeout: std::time::Duration::from_secs(30),
            upstream_semaphore: Arc::new(tokio::sync::Semaphore::new(100)),
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
