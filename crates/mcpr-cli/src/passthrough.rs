use std::time::Instant;

use axum::{
    body::{Body, Bytes},
    http::{HeaderMap, Method, StatusCode, header},
    response::{IntoResponse, Response},
};

use crate::AppState;
use crate::logger::LogEntry;
use crate::proxy::forward_request;
use mcpr_core::forwarding::{build_response, read_body_capped};
use mcpr_core::sse::split_upstream;

/// Serve the OAuth callback relay page.
///
/// When the MCP server's OAuth flow redirects back to this proxy with an
/// authorization code, this page forwards the code/state to the cloud Studio's
/// own `/studio/oauth/callback` endpoint, which handles the token exchange.
pub async fn serve_oauth_callback_relay() -> Response {
    let html = r#"<!DOCTYPE html>
<html><head><meta charset="utf-8"><title>Authorization</title></head>
<body>
<div id="msg" style="display:flex;align-items:center;justify-content:center;height:100vh;font-family:system-ui;color:#888">
<p>Completing authorization…</p>
</div>
<script>
(function() {
  var params = new URLSearchParams(window.location.search);
  var studioOrigin = params.get("studio");
  if (!studioOrigin) {
    var host = window.location.hostname;
    if (host === "localhost" || host === "127.0.0.1") {
      studioOrigin = window.location.protocol + "//localhost:5173";
    } else {
      studioOrigin = "https://cloud.mcpr.app";
    }
  }
  var callbackUrl = studioOrigin.replace(/\/+$/, "") + "/studio/oauth/callback?" + params.toString();
  window.location.replace(callbackUrl);
})();
</script>
</body></html>"#.to_string();

    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        "text/html; charset=utf-8".parse().unwrap(),
    );
    headers.insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    (StatusCode::OK, headers, html).into_response()
}

/// Forward a request to upstream and return the response, rewriting upstream URLs in JSON bodies.
pub async fn forward_and_passthrough(
    state: &AppState,
    url: &str,
    method: Method,
    log_path: &str,
    headers: &HeaderMap,
    body: &Bytes,
    start: Instant,
) -> Response {
    let is_streaming = headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .map(|a| a.contains("text/event-stream"))
        .unwrap_or(false);
    let upstream_start = Instant::now();
    match forward_request(state, url, method.clone(), headers, body, is_streaming).await {
        Ok(resp) => {
            let status = resp.status().as_u16();
            let resp_headers = resp.headers().clone();
            let bytes = match read_body_capped(resp, state.max_response_body).await {
                Ok(b) => b,
                Err(err_resp) => return err_resp,
            };
            let upstream_ms = upstream_start.elapsed().as_millis() as u64;

            // Rewrite upstream base URL → proxy URL in JSON responses
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
                state.logger.emit(
                    LogEntry::new(method.as_str(), log_path, status, "rewritten")
                        .upstream(url)
                        .size(rewritten_bytes.len())
                        .upstream_duration(upstream_ms)
                        .duration(start),
                );
                build_response(status, &resp_headers, Body::from(rewritten_bytes))
            } else {
                state.logger.emit(
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
            state.logger.emit(
                LogEntry::new(method.as_str(), log_path, 502, "upstream error")
                    .upstream(url)
                    .upstream_duration(upstream_ms)
                    .duration(start),
            );
            (StatusCode::BAD_GATEWAY, format!("Upstream error: {e}")).into_response()
        }
    }
}
