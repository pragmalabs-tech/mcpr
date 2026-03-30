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
use serde_json::Value;

use crate::AppState;
use crate::display::log_request;
use crate::jsonrpc::{self, McpMethod};
use crate::rewrite::rewrite_response;
use crate::session::{self, SessionState, SessionStore};
use crate::tui::state::LogEntry;
use crate::widgets::{
    fetch_widget_html, list_widgets, serve_studio, serve_widget_asset, serve_widget_html,
};

/// Read a response body with a size cap. Returns 502 if the upstream response exceeds `max_bytes`.
async fn read_body_capped(resp: reqwest::Response, max_bytes: usize) -> Result<Bytes, Response> {
    // Early reject if Content-Length is known and too large
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
/// SSE format: `data: {...}\n\n` — possibly multiple events.
/// Returns (json_bytes, is_sse) so we can re-wrap after rewriting.
fn extract_json_from_sse(bytes: &[u8]) -> Option<Vec<u8>> {
    let text = std::str::from_utf8(bytes).ok()?;
    // Quick check: does it look like SSE?
    if !text.trim_start().starts_with("data:") && !text.contains("\ndata:") {
        return None;
    }
    // Extract all data: lines and join them
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
        None // Multiple data events or none — don't try to parse
    }
}

/// Re-wrap JSON bytes into SSE format.
fn wrap_as_sse(json_bytes: &[u8]) -> Vec<u8> {
    let mut out = b"data: ".to_vec();
    out.extend_from_slice(json_bytes);
    out.extend_from_slice(b"\n\n");
    out
}

/// Split a full upstream URL into (base, path).
/// e.g. "http://localhost:9000/mcp" → ("http://localhost:9000", "/mcp")
/// e.g. "http://localhost:9000" → ("http://localhost:9000", "")
fn split_upstream(url: &str) -> (&str, &str) {
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

/// Check if a POST body is a valid JSON-RPC 2.0 message (MCP request).
/// This is the definitive MCP detection: parse the body and check for "jsonrpc": "2.0".
/// OAuth, form-encoded, and other non-JSON-RPC POSTs will return None.
fn parse_mcp_body(body: &Bytes) -> Option<jsonrpc::ParsedBody> {
    jsonrpc::parse_body(body)
}

/// Check if a GET request is an MCP SSE call based on accept header.
fn is_mcp_sse(headers: &HeaderMap) -> bool {
    headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .map(|a| a.contains("text/event-stream"))
        .unwrap_or(false)
}

/// Check if a request is for a static widget asset.
/// Uses both file extension and Accept header to decide.
fn is_widget_asset(path: &str, headers: &HeaderMap) -> bool {
    // Check file extension first
    let ext = path.rsplit('.').next().unwrap_or("");
    if matches!(
        ext,
        "js" | "mjs"
            | "css"
            | "html"
            | "svg"
            | "png"
            | "jpg"
            | "jpeg"
            | "gif"
            | "ico"
            | "woff"
            | "woff2"
            | "ttf"
            | "eot"
            | "map"
            | "webp"
    ) {
        return true;
    }

    // Check Accept header for static content types
    if let Some(accept) = headers.get(header::ACCEPT).and_then(|v| v.to_str().ok())
        && (accept.contains("text/html")
            || accept.contains("text/css")
            || accept.contains("image/")
            || accept.contains("font/")
            || accept.contains("application/javascript"))
    {
        return true;
    }

    false
}

/// All proxy routes — catch-all that routes by method + content-type.
pub fn proxy_routes(router: Router<AppState>) -> Router<AppState> {
    router.fallback(any(handle_request))
}

/// Catch-all handler: route by method and content-type.
///
/// Routing priority:
/// 1. Static widget asset (file ext or Accept header) → widget source
/// 2. MCP JSON-RPC POST (application/json) → MCP upstream (with rewriting)
/// 3. MCP SSE GET (text/event-stream) → MCP upstream (streaming)
/// 4. Everything else (DELETE, well-known, oauth, etc.) → MCP upstream (passthrough)
async fn handle_request(
    State(state): State<AppState>,
    method: Method,
    headers: HeaderMap,
    uri: axum::http::Uri,
    body: Bytes,
) -> Response {
    let start = Instant::now();
    let path = uri.path();

    // 0. Studio SPA (bundled)
    if method == Method::GET && (path == "/studio" || path.starts_with("/studio/")) {
        return serve_studio(path).await;
    }

    // 1. Widget endpoints: /widgets/{name}.html?raw=1, /widgets (JSON), /widgets/preview → studio
    if method == Method::GET {
        if path == "/widgets/preview" || path == "/widgets/preview/" {
            // Redirect to studio
            return axum::response::Redirect::permanent("/studio").into_response();
        }
        if let Some(name) = path
            .strip_prefix("/widgets/")
            .and_then(|s| s.strip_suffix(".html"))
        {
            let query = uri.query().unwrap_or("");
            if query.contains("debug=1") {
                // Redirect to studio widget page
                let redirect = format!("/studio/#/widgets/{name}");
                return axum::response::Redirect::temporary(&redirect).into_response();
            }
            let raw = query.contains("raw=1");
            return serve_widget_html(&state, name, raw).await;
        }
        if path == "/widgets" || path == "/widgets/" {
            return list_widgets(&state).await;
        }
    }

    // 1. Static widget assets → widget source (check first, before MCP)
    if method == Method::GET && state.widget_source.is_some() && is_widget_asset(path, &headers) {
        return serve_widget_asset(&state, path).await;
    }

    // 2. MCP JSON-RPC POST — detect by body ("jsonrpc": "2.0"), not headers.
    // Non-JSON-RPC POSTs (OAuth /register, /token) fall through to passthrough (rule 4).
    if method == Method::POST
        && let Some(parsed) = parse_mcp_body(&body)
    {
        return handle_mcp_post(&state, path, &headers, &body, parsed, start).await;
    }

    // 3. MCP SSE GET (text/event-stream) → stream from mcp_upstream
    if method == Method::GET && is_mcp_sse(&headers) {
        let upstream_url = state.mcp_upstream.trim_end_matches('/').to_string();
        let upstream_start = Instant::now();
        return match forward_request(
            &state,
            &upstream_url,
            method.clone(),
            &headers,
            &Bytes::new(),
        )
        .await
        {
            Ok(resp) => {
                let upstream_ms = upstream_start.elapsed().as_millis() as u64;
                let status = resp.status().as_u16();
                let resp_headers = resp.headers().clone();
                log_request(
                    &state.tui_state,
                    LogEntry::new("GET", path, status, "sse")
                        .upstream(&upstream_url)
                        .upstream_duration(upstream_ms)
                        .duration(start),
                );
                build_response(
                    status,
                    &resp_headers,
                    Body::from_stream(resp.bytes_stream()),
                )
            }
            Err(e) => {
                let upstream_ms = upstream_start.elapsed().as_millis() as u64;
                log_request(
                    &state.tui_state,
                    LogEntry::new("GET", path, 502, "upstream error")
                        .upstream(&upstream_url)
                        .upstream_duration(upstream_ms)
                        .duration(start),
                );
                (StatusCode::BAD_GATEWAY, format!("Upstream error: {e}")).into_response()
            }
        };
    }

    // 4. Everything else (DELETE, well-known, oauth, GET, POST) → MCP upstream + path
    let (base, _) = split_upstream(&state.mcp_upstream);
    let upstream_url = format!("{}{}", base.trim_end_matches('/'), path);
    forward_and_passthrough(&state, &upstream_url, method, path, &headers, &body, start).await
}

/// Handle MCP JSON-RPC POST — intercept resources/read, forward, rewrite response.
/// The body has already been validated as JSON-RPC 2.0 by `parse_mcp_body`.
async fn handle_mcp_post(
    state: &AppState,
    path: &str,
    headers: &HeaderMap,
    body: &Bytes,
    parsed: jsonrpc::ParsedBody,
    start: Instant,
) -> Response {
    let mcp_method = parsed.mcp_method();
    let method_str = mcp_method.as_str();
    let call_detail = parsed.detail();

    // Extract client info from initialize request before forwarding
    let client_info = if mcp_method == McpMethod::Initialize {
        parsed.first_params().and_then(session::parse_client_info)
    } else {
        None
    };

    // Track session activity and state transitions
    let req_session_id = headers
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    if let Some(ref sid) = req_session_id {
        state.sessions.touch(sid).await;
        if mcp_method == McpMethod::Initialized {
            state.sessions.update_state(sid, SessionState::Active).await;
        }
    }

    // Intercept resources/read for widget HTML serving (single requests only)
    if mcp_method == McpMethod::ResourcesRead && !parsed.is_batch && state.widget_source.is_some() {
        // Re-parse as Value for the interception logic
        if let Ok(json_val) = serde_json::from_slice::<Value>(body)
            && let Some(response) =
                handle_resources_read(state, headers, body, &json_val, start).await
        {
            return response;
        }
    }

    // Forward to upstream MCP URL
    let upstream_url = state.mcp_upstream.trim_end_matches('/').to_string();
    let upstream_start = Instant::now();
    let resp = match forward_request(state, &upstream_url, Method::POST, headers, body).await {
        Ok(r) => r,
        Err(e) => {
            let upstream_ms = upstream_start.elapsed().as_millis() as u64;
            log_request(
                &state.tui_state,
                LogEntry::new("POST", path, 502, "upstream error")
                    .mcp_method(method_str)
                    .maybe_detail(call_detail.as_deref())
                    .maybe_session_id(req_session_id.as_deref())
                    .upstream(&upstream_url)
                    .upstream_duration(upstream_ms)
                    .duration(start),
            );
            return (StatusCode::BAD_GATEWAY, format!("Upstream error: {e}")).into_response();
        }
    };

    let status = resp.status().as_u16();
    let resp_headers = resp.headers().clone();

    // Track session from upstream response — for initialize, the session ID is in the response
    let resp_session_id = resp_headers
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .map(String::from);
    if mcp_method == McpMethod::Initialize
        && status < 400
        && let Some(ref sid) = resp_session_id
    {
        state.sessions.create(sid).await;
        state
            .sessions
            .update_state(sid, SessionState::Initialized)
            .await;
        if let Some(info) = client_info {
            state.sessions.set_client_info(sid, info).await;
        }
    }
    // Use response session ID (for initialize) or request session ID (for everything else)
    let log_session_id = resp_session_id.or(req_session_id);

    // Collect full body for rewriting (POST SSE is finite), capped to prevent OOM
    let resp_bytes = match read_body_capped(resp, state.max_response_body).await {
        Ok(b) => b,
        Err(err_resp) => return err_resp,
    };
    let upstream_ms = upstream_start.elapsed().as_millis() as u64;
    let config = state.rewrite_config.read().await;

    // Try to parse and rewrite JSON response (may be SSE-wrapped)
    let (json_bytes, is_sse) = match extract_json_from_sse(&resp_bytes) {
        Some(extracted) => (extracted, true),
        None => (resp_bytes.to_vec(), false),
    };

    if let Ok(mut json_body) = serde_json::from_slice::<Value>(&json_bytes) {
        // Check for JSON-RPC error in response body (extract before mutable rewrite)
        let rpc_error =
            jsonrpc::extract_error_code(&json_body).map(|(code, msg)| (code, msg.to_string()));

        rewrite_response(method_str, &mut json_body, &config);
        let rewritten = serde_json::to_vec(&json_body).unwrap_or(json_bytes);
        let body = if is_sse {
            wrap_as_sse(&rewritten)
        } else {
            rewritten
        };
        let note = if is_sse { "rewritten+sse" } else { "rewritten" };
        let mut entry = LogEntry::new("POST", path, status, note)
            .mcp_method(method_str)
            .maybe_detail(call_detail.as_deref())
            .maybe_session_id(log_session_id.as_deref())
            .upstream(&upstream_url)
            .size(body.len())
            .upstream_duration(upstream_ms)
            .duration(start);
        if let Some((code, ref msg)) = rpc_error {
            entry = entry.jsonrpc_error(code, msg);
        }
        log_request(&state.tui_state, entry);
        build_response(status, &resp_headers, Body::from(body))
    } else {
        log_request(
            &state.tui_state,
            LogEntry::new("POST", path, status, "passthrough")
                .mcp_method(method_str)
                .maybe_detail(call_detail.as_deref())
                .maybe_session_id(log_session_id.as_deref())
                .upstream(&upstream_url)
                .size(resp_bytes.len())
                .upstream_duration(upstream_ms)
                .duration(start),
        );
        build_response(status, &resp_headers, Body::from(resp_bytes))
    }
}

/// Handle resources/read interception: serve local widget HTML + upstream metadata.
async fn handle_resources_read(
    state: &AppState,
    headers: &HeaderMap,
    raw_body: &Bytes,
    parsed: &Value,
    start: Instant,
) -> Option<Response> {
    let uri = parsed
        .get("params")
        .and_then(|p| p.get("uri"))
        .and_then(|u| u.as_str())?;

    let widget_name = uri.strip_prefix("ui://widget/")?;
    let widget_name = widget_name.trim_end_matches(".html");

    let html = fetch_widget_html(state, widget_name).await?;

    // Forward to upstream to get the metadata
    let upstream_url = state.mcp_upstream.trim_end_matches('/').to_string();
    let upstream_start = Instant::now();
    let upstream_resp = forward_request(state, &upstream_url, Method::POST, headers, raw_body)
        .await
        .ok()?;

    let upstream_bytes = read_body_capped(upstream_resp, state.max_response_body)
        .await
        .ok()?;
    let upstream_ms = upstream_start.elapsed().as_millis() as u64;
    let json_bytes =
        extract_json_from_sse(&upstream_bytes).unwrap_or_else(|| upstream_bytes.to_vec());
    let mut json_body: Value = serde_json::from_slice(&json_bytes).ok()?;

    // Replace the HTML text with our local version
    if let Some(contents) = json_body
        .get_mut("result")
        .and_then(|r| r.get_mut("contents"))
        .and_then(|c| c.as_array_mut())
    {
        for content in contents.iter_mut() {
            if content.get("text").is_some() {
                content["text"] = Value::String(html.clone());
            }
        }
    }

    let config = state.rewrite_config.read().await;
    rewrite_response(jsonrpc::RESOURCES_READ, &mut json_body, &config);
    drop(config);

    let body = serde_json::to_vec(&json_body).unwrap_or_default();
    log_request(
        &state.tui_state,
        LogEntry::new("POST", "/*", 200, "intercepted")
            .mcp_method(jsonrpc::RESOURCES_READ)
            .size(body.len())
            .upstream_duration(upstream_ms)
            .duration(start),
    );
    let mut resp_headers = HeaderMap::new();
    resp_headers.insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
    Some(build_response(200, &resp_headers, Body::from(body)))
}

/// Forward a request and return response, rewriting upstream URLs in JSON bodies.
async fn forward_and_passthrough(
    state: &AppState,
    url: &str,
    method: Method,
    log_path: &str,
    headers: &HeaderMap,
    body: &Bytes,
    start: Instant,
) -> Response {
    let upstream_start = Instant::now();
    match forward_request(state, url, method.clone(), headers, body).await {
        Ok(resp) => {
            let status = resp.status().as_u16();
            let resp_headers = resp.headers().clone();
            let bytes = match read_body_capped(resp, state.max_response_body).await {
                Ok(b) => b,
                Err(err_resp) => return err_resp,
            };
            let upstream_ms = upstream_start.elapsed().as_millis() as u64;

            // Rewrite upstream base URL → proxy URL in JSON responses
            // (e.g. OAuth metadata: registration_endpoint, issuer, etc.)
            let is_json = resp_headers
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .map(|ct| ct.contains("json"))
                .unwrap_or(false);

            if is_json {
                let config = state.rewrite_config.read().await;
                let (upstream_base, _) = split_upstream(&config.mcp_upstream);
                let body_str = String::from_utf8_lossy(&bytes);
                let rewritten = body_str.replace(
                    upstream_base.trim_end_matches('/'),
                    config.proxy_url.trim_end_matches('/'),
                );
                drop(config);
                let rewritten_bytes = rewritten.into_bytes();
                log_request(
                    &state.tui_state,
                    LogEntry::new(method.as_str(), log_path, status, "rewritten")
                        .upstream(url)
                        .size(rewritten_bytes.len())
                        .upstream_duration(upstream_ms)
                        .duration(start),
                );
                build_response(status, &resp_headers, Body::from(rewritten_bytes))
            } else {
                log_request(
                    &state.tui_state,
                    LogEntry::new(method.as_str(), log_path, status, "passthrough")
                        .upstream(url)
                        .size(bytes.len())
                        .upstream_duration(upstream_ms)
                        .duration(start),
                );
                build_response(status, &resp_headers, Body::from(bytes))
            }
        }
        Err(e) => {
            let upstream_ms = upstream_start.elapsed().as_millis() as u64;
            log_request(
                &state.tui_state,
                LogEntry::new(method.as_str(), log_path, 502, "upstream error")
                    .upstream(url)
                    .upstream_duration(upstream_ms)
                    .duration(start),
            );
            (StatusCode::BAD_GATEWAY, format!("Upstream error: {e}")).into_response()
        }
    }
}

/// Send a request to the upstream server, forwarding relevant headers.
async fn forward_request(
    state: &AppState,
    url: &str,
    method: Method,
    headers: &HeaderMap,
    body: &Bytes,
) -> Result<reqwest::Response, reqwest::Error> {
    let mut req = state.http_client.request(method, url);

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
fn build_response(status: u16, upstream_headers: &HeaderMap, body: Body) -> Response {
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

    // ── parse_mcp_body (JSON-RPC 2.0 detection) ──

    #[test]
    fn detect_mcp_jsonrpc_request() {
        let body = br#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
        let parsed = parse_mcp_body(&Bytes::from_static(body));
        assert!(parsed.is_some());
        let p = parsed.unwrap();
        assert_eq!(p.method_str(), "tools/list");
        assert!(!p.is_batch);
    }

    #[test]
    fn detect_mcp_jsonrpc_notification() {
        let body = br#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;
        let parsed = parse_mcp_body(&Bytes::from_static(body));
        assert!(parsed.is_some());
        assert!(parsed.unwrap().is_notification_only());
    }

    #[test]
    fn detect_mcp_jsonrpc_batch() {
        let body = br#"[{"jsonrpc":"2.0","id":1,"method":"tools/list"},{"jsonrpc":"2.0","id":2,"method":"resources/list"}]"#;
        let parsed = parse_mcp_body(&Bytes::from_static(body));
        assert!(parsed.is_some());
        assert!(parsed.unwrap().is_batch);
    }

    #[test]
    fn reject_oauth_register_json() {
        // OAuth /register POST — valid JSON but not JSON-RPC
        let body = br#"{"client_name":"My App","redirect_uris":["https://example.com/cb"]}"#;
        assert!(parse_mcp_body(&Bytes::from_static(body)).is_none());
    }

    #[test]
    fn reject_form_encoded() {
        let body = b"grant_type=client_credentials&client_id=abc";
        assert!(parse_mcp_body(&Bytes::from_static(body)).is_none());
    }

    #[test]
    fn reject_empty_body() {
        assert!(parse_mcp_body(&Bytes::new()).is_none());
    }

    #[test]
    fn reject_wrong_jsonrpc_version() {
        let body = br#"{"jsonrpc":"1.0","id":1,"method":"test"}"#;
        assert!(parse_mcp_body(&Bytes::from_static(body)).is_none());
    }

    // ── is_mcp_sse ──

    #[test]
    fn is_mcp_sse_accept() {
        let mut headers = HeaderMap::new();
        headers.insert(header::ACCEPT, "text/event-stream".parse().unwrap());
        assert!(is_mcp_sse(&headers));
    }

    #[test]
    fn is_not_mcp_sse_html() {
        let mut headers = HeaderMap::new();
        headers.insert(header::ACCEPT, "text/html".parse().unwrap());
        assert!(!is_mcp_sse(&headers));
    }

    #[test]
    fn is_not_mcp_sse_no_accept() {
        let headers = HeaderMap::new();
        assert!(!is_mcp_sse(&headers));
    }

    // ── is_widget_asset ──

    #[test]
    fn widget_asset_by_js_ext() {
        let headers = HeaderMap::new();
        assert!(is_widget_asset("/assets/main.js", &headers));
    }

    #[test]
    fn widget_asset_by_css_ext() {
        let headers = HeaderMap::new();
        assert!(is_widget_asset("/styles/app.css", &headers));
    }

    #[test]
    fn widget_asset_by_woff2_ext() {
        let headers = HeaderMap::new();
        assert!(is_widget_asset("/fonts/inter.woff2", &headers));
    }

    #[test]
    fn widget_asset_by_svg_ext() {
        let headers = HeaderMap::new();
        assert!(is_widget_asset("/icons/logo.svg", &headers));
    }

    #[test]
    fn widget_asset_by_accept_html() {
        let mut headers = HeaderMap::new();
        headers.insert(header::ACCEPT, "text/html".parse().unwrap());
        assert!(is_widget_asset("/some-path", &headers));
    }

    #[test]
    fn widget_asset_by_accept_image() {
        let mut headers = HeaderMap::new();
        headers.insert(header::ACCEPT, "image/png".parse().unwrap());
        assert!(is_widget_asset("/logo", &headers));
    }

    #[test]
    fn widget_asset_by_accept_javascript() {
        let mut headers = HeaderMap::new();
        headers.insert(header::ACCEPT, "application/javascript".parse().unwrap());
        assert!(is_widget_asset("/bundle", &headers));
    }

    #[test]
    fn not_widget_asset_well_known() {
        let headers = HeaderMap::new();
        assert!(!is_widget_asset(
            "/.well-known/oauth-authorization-server",
            &headers
        ));
    }

    #[test]
    fn not_widget_asset_mcp() {
        let headers = HeaderMap::new();
        assert!(!is_widget_asset("/mcp", &headers));
    }

    #[test]
    fn not_widget_asset_token() {
        let headers = HeaderMap::new();
        assert!(!is_widget_asset("/token", &headers));
    }

    #[test]
    fn not_widget_asset_authorize() {
        let headers = HeaderMap::new();
        assert!(!is_widget_asset("/authorize", &headers));
    }

    #[test]
    fn not_widget_asset_register() {
        let headers = HeaderMap::new();
        assert!(!is_widget_asset("/register", &headers));
    }

    #[test]
    fn not_widget_asset_json_accept() {
        let mut headers = HeaderMap::new();
        headers.insert(header::ACCEPT, "application/json".parse().unwrap());
        assert!(!is_widget_asset("/some-path", &headers));
    }

    #[test]
    fn not_widget_asset_sse_accept() {
        let mut headers = HeaderMap::new();
        headers.insert(header::ACCEPT, "text/event-stream".parse().unwrap());
        assert!(!is_widget_asset("/mcp", &headers));
    }

    // ── DefaultBodyLimit integration ──

    /// Create a test AppState pointing at the given upstream URL.
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
            rewrite_config: Arc::new(RwLock::new(crate::rewrite::RewriteConfig {
                proxy_url: "http://localhost:0".to_string(),
                proxy_domain: "localhost".to_string(),
                mcp_upstream: upstream_url.to_string(),
                extra_csp_domains: vec![],
                csp_mode: crate::config::CspMode::default(),
            })),
            http_client: reqwest::Client::new(),
            tui_state: crate::tui::new_shared_state(),
            sessions: crate::session::MemorySessionStore::new(),
            max_request_body: max_request,
            max_response_body: max_response,
        }
    }

    #[tokio::test]
    async fn body_limit_rejects_oversized_request() {
        use axum::routing::post;

        // Mock upstream that echoes back
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

        // Small JSON-RPC body (under 1KB) → should reach upstream → 200
        let small_body = br#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#;
        let resp = client
            .post(&url)
            .header("content-type", "application/json")
            .body(small_body.to_vec())
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);

        // Large body (2KB, over limit) → 413
        let large_body = vec![b'x'; 2048];
        let resp = client.post(&url).body(large_body).send().await.unwrap();
        assert_eq!(resp.status(), 413);
    }

    #[tokio::test]
    async fn response_body_cap_rejects_oversized_upstream() {
        use axum::routing::post;

        // Upstream that returns a large response body (2KB)
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

        // Proxy with 1KB response body cap
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
