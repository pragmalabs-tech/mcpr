use axum::{
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::AppState;
use crate::display::log_request;
use crate::tui::state::LogEntry;

// ── Types ───────────────────────────────────────────────

#[derive(Clone)]
pub enum WidgetSource {
    /// Reverse proxy to a running server (e.g., http://localhost:4444)
    Proxy(String),
    /// Serve static files from a directory (e.g., ./widgets/dist)
    Static(String),
}

// ── Asset serving ───────────────────────────────────────

/// Serve a widget asset by path. Called from proxy's catch-all handler for static asset requests.
pub async fn serve_widget_asset(state: &AppState, path: &str) -> Response {
    match &state.widget_source {
        Some(WidgetSource::Proxy(base_url)) => {
            let url = format!("{}{}", base_url.trim_end_matches('/'), path);
            match state
                .http_client
                .get(&url)
                .timeout(Duration::from_secs(10))
                .send()
                .await
            {
                Ok(resp) => {
                    let status = resp.status().as_u16();
                    let mut headers = HeaderMap::new();
                    if let Some(ct) = resp.headers().get(header::CONTENT_TYPE) {
                        headers.insert(header::CONTENT_TYPE, ct.clone());
                    }
                    headers.insert(header::CACHE_CONTROL, "no-cache".parse().unwrap());
                    let status_code =
                        StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
                    let bytes = resp.bytes().await.unwrap_or_default();
                    log_request(
                        &state.tui_state,
                        LogEntry::new("GET", path, status, "widget")
                            .upstream(&url)
                            .size(bytes.len()),
                    );
                    (status_code, headers, bytes).into_response()
                }
                Err(e) => {
                    log_request(
                        &state.tui_state,
                        LogEntry::new("GET", path, 502, "widget error").upstream(&url),
                    );
                    (StatusCode::BAD_GATEWAY, format!("Widget proxy error: {e}")).into_response()
                }
            }
        }
        Some(WidgetSource::Static(dir)) => {
            let file_path = PathBuf::from(dir).join(path.trim_start_matches('/'));
            match tokio::fs::read(&file_path).await {
                Ok(bytes) => {
                    log_request(
                        &state.tui_state,
                        LogEntry::new("GET", path, 200, "widget")
                            .upstream(file_path.to_str().unwrap_or(path))
                            .size(bytes.len()),
                    );
                    let mime = mime_from_path(&file_path);
                    let mut headers = HeaderMap::new();
                    headers.insert(header::CONTENT_TYPE, mime.parse().unwrap());
                    headers.insert(header::CACHE_CONTROL, "no-cache".parse().unwrap());
                    (StatusCode::OK, headers, bytes).into_response()
                }
                Err(_) => {
                    log_request(
                        &state.tui_state,
                        LogEntry::new("GET", path, 404, "not found"),
                    );
                    StatusCode::NOT_FOUND.into_response()
                }
            }
        }
        None => {
            log_request(
                &state.tui_state,
                LogEntry::new("GET", path, 404, "no widget source"),
            );
            StatusCode::NOT_FOUND.into_response()
        }
    }
}

// ── Widget HTML ─────────────────────────────────────────

/// Fetch widget HTML for a given widget name (used by resources/read interception).
/// Asset URLs are made absolute so they resolve through the tunnel, not the sandbox origin.
pub async fn fetch_widget_html(state: &AppState, widget_name: &str) -> Option<String> {
    let html = match &state.widget_source {
        Some(WidgetSource::Proxy(base_url)) => {
            let url = format!(
                "{}/src/{}/index.html",
                base_url.trim_end_matches('/'),
                widget_name
            );
            let resp = state
                .http_client
                .get(&url)
                .timeout(Duration::from_secs(10))
                .send()
                .await
                .ok()?;
            if !resp.status().is_success() {
                return None;
            }
            resp.text().await.ok()?
        }
        Some(WidgetSource::Static(dir)) => {
            let path = PathBuf::from(dir).join(format!("src/{widget_name}/index.html"));
            tokio::fs::read_to_string(&path).await.ok()?
        }
        None => return None,
    };

    // Make absolute paths point to our tunnel, not the sandbox origin
    let config = state.rewrite_config.read().await;
    let proxy = config.proxy_url.trim_end_matches('/');
    Some(rewrite_html_asset_urls(&html, proxy))
}

/// Serve raw widget HTML at `/widgets/{name}.html?raw=1`.
/// Without `?raw=1`, redirects to studio.
pub async fn serve_widget_html(state: &AppState, name: &str, raw: bool) -> Response {
    if !raw {
        let redirect = format!("/studio/#/widgets/{name}");
        return axum::response::Redirect::temporary(&redirect).into_response();
    }

    let Some(html) = fetch_widget_html(state, name).await else {
        log_request(
            &state.tui_state,
            LogEntry::new("GET", &format!("/widgets/{name}.html"), 404, "not found"),
        );
        return (StatusCode::NOT_FOUND, format!("Widget '{name}' not found")).into_response();
    };

    log_request(
        &state.tui_state,
        LogEntry::new("GET", &format!("/widgets/{name}.html"), 200, "widget raw").size(html.len()),
    );
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CONTENT_TYPE,
        "text/html; charset=utf-8".parse().unwrap(),
    );
    headers.insert(header::CACHE_CONTROL, "no-cache".parse().unwrap());
    (StatusCode::OK, headers, html).into_response()
}

// ── Widget listing ──────────────────────────────────────

/// JSON list of available widgets at `/widgets`.
pub async fn list_widgets(state: &AppState) -> Response {
    let names = discover_widget_names(state).await;
    let body = serde_json::json!({
        "widgets": names.iter().map(|n| {
            serde_json::json!({
                "name": n,
                "url": format!("/widgets/{n}.html"),
            })
        }).collect::<Vec<_>>(),
    });
    log_request(
        &state.tui_state,
        LogEntry::new("GET", "/widgets", 200, &format!("{} widgets", names.len())),
    );
    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
    (
        StatusCode::OK,
        headers,
        serde_json::to_string(&body).unwrap(),
    )
        .into_response()
}

// ── Widget discovery ────────────────────────────────────

/// Discover available widget names from the widget source.
pub async fn discover_widget_names(state: &AppState) -> Vec<String> {
    match &state.widget_source {
        Some(WidgetSource::Static(dir)) => {
            let src_dir = PathBuf::from(dir).join("src");
            let Ok(entries) = std::fs::read_dir(&src_dir) else {
                return vec![];
            };
            let mut names: Vec<String> = entries
                .filter_map(|e| e.ok())
                .filter(|e| e.path().join("index.html").exists())
                .filter_map(|e| e.file_name().into_string().ok())
                .collect();
            names.sort();
            names
        }
        Some(WidgetSource::Proxy(base_url)) => {
            let base = base_url.trim_end_matches('/');
            let candidates = ["goal_detail", "question", "question_review", "vocab_review"];
            let mut found = vec![];
            for name in &candidates {
                let url = format!("{base}/src/{name}/index.html");
                if let Ok(resp) = state
                    .http_client
                    .head(&url)
                    .timeout(Duration::from_secs(10))
                    .send()
                    .await
                    && resp.status().is_success()
                {
                    found.push(name.to_string());
                }
            }
            found
        }
        None => vec![],
    }
}

// ── Studio (fully embedded SPA) ─────────────────────────

use include_dir::{Dir, include_dir};

static STUDIO_DIR: Dir = include_dir!("static/studio");

/// Serve the bundled studio SPA. Everything is embedded in the binary.
pub async fn serve_studio(path: &str) -> Response {
    let sub = path
        .strip_prefix("/studio")
        .unwrap_or("")
        .trim_start_matches('/');

    // Non-empty path with extension → try to serve the file
    let file_path = if sub.is_empty() { "index.html" } else { sub };

    if let Some(file) = STUDIO_DIR.get_file(file_path) {
        let mime = mime_from_ext(
            std::path::Path::new(file_path)
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or(""),
        );
        let cache = if file_path.starts_with("assets/") {
            "public, max-age=31536000, immutable"
        } else {
            "no-cache"
        };
        let mut headers = HeaderMap::new();
        headers.insert(header::CONTENT_TYPE, mime.parse().unwrap());
        headers.insert(header::CACHE_CONTROL, cache.parse().unwrap());
        return (StatusCode::OK, headers, file.contents()).into_response();
    }

    // SPA fallback: serve index.html for client-side routing
    if let Some(index) = STUDIO_DIR.get_file("index.html") {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONTENT_TYPE,
            "text/html; charset=utf-8".parse().unwrap(),
        );
        headers.insert(header::CACHE_CONTROL, "no-cache".parse().unwrap());
        return (StatusCode::OK, headers, index.contents()).into_response();
    }

    StatusCode::NOT_FOUND.into_response()
}

fn mime_from_ext(ext: &str) -> &'static str {
    match ext {
        "html" => "text/html; charset=utf-8",
        "js" => "application/javascript",
        "css" => "text/css",
        "svg" => "image/svg+xml",
        "json" => "application/json",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "ico" => "image/x-icon",
        _ => "application/octet-stream",
    }
}

// ── Helpers ─────────────────────────────────────────────

fn mime_from_path(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("js") => "application/javascript",
        Some("css") => "text/css",
        Some("html") => "text/html",
        Some("svg") => "image/svg+xml",
        Some("json") => "application/json",
        Some("woff") => "font/woff",
        Some("woff2") => "font/woff2",
        Some("ttf") => "font/ttf",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        _ => "application/octet-stream",
    }
}

/// Rewrite asset URLs in HTML to point through the proxy.
pub(crate) fn rewrite_html_asset_urls(html: &str, proxy_url: &str) -> String {
    html.replace("\"/", &format!("\"{proxy_url}/"))
        .replace("'/", &format!("'{proxy_url}/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── MIME type detection ──

    #[test]
    fn mime_js() {
        assert_eq!(
            mime_from_path(&PathBuf::from("app.js")),
            "application/javascript"
        );
    }

    #[test]
    fn mime_css() {
        assert_eq!(mime_from_path(&PathBuf::from("style.css")), "text/css");
    }

    #[test]
    fn mime_html() {
        assert_eq!(mime_from_path(&PathBuf::from("index.html")), "text/html");
    }

    #[test]
    fn mime_svg() {
        assert_eq!(mime_from_path(&PathBuf::from("icon.svg")), "image/svg+xml");
    }

    #[test]
    fn mime_woff2() {
        assert_eq!(mime_from_path(&PathBuf::from("font.woff2")), "font/woff2");
    }

    #[test]
    fn mime_jpeg_variants() {
        assert_eq!(mime_from_path(&PathBuf::from("photo.jpg")), "image/jpeg");
        assert_eq!(mime_from_path(&PathBuf::from("photo.jpeg")), "image/jpeg");
    }

    #[test]
    fn mime_unknown_extension() {
        assert_eq!(
            mime_from_path(&PathBuf::from("file.xyz")),
            "application/octet-stream"
        );
    }

    #[test]
    fn mime_no_extension() {
        assert_eq!(
            mime_from_path(&PathBuf::from("Makefile")),
            "application/octet-stream"
        );
    }

    // ── HTML asset URL rewriting ──

    #[test]
    fn rewrite_double_quote_absolute_paths() {
        let html = r#"<script src="/assets/main.js"></script>"#;
        let result = rewrite_html_asset_urls(html, "https://abc.tunnel.example.com");
        assert_eq!(
            result,
            r#"<script src="https://abc.tunnel.example.com/assets/main.js"></script>"#
        );
    }

    #[test]
    fn rewrite_single_quote_absolute_paths() {
        let html = "<link href='/styles/app.css'>";
        let result = rewrite_html_asset_urls(html, "https://abc.tunnel.example.com");
        assert_eq!(
            result,
            "<link href='https://abc.tunnel.example.com/styles/app.css'>"
        );
    }

    #[test]
    fn rewrite_preserves_relative_paths() {
        let html = r#"<script src="./local.js"></script>"#;
        let result = rewrite_html_asset_urls(html, "https://abc.tunnel.example.com");
        assert_eq!(result, r#"<script src="./local.js"></script>"#);
    }

    #[test]
    fn rewrite_preserves_external_urls() {
        let html = r#"<script src="https://cdn.example.com/lib.js"></script>"#;
        let result = rewrite_html_asset_urls(html, "https://abc.tunnel.example.com");
        assert_eq!(
            result,
            r#"<script src="https://cdn.example.com/lib.js"></script>"#
        );
    }

    #[test]
    fn rewrite_multiple_paths() {
        let html = r#"<script src="/js/a.js"></script><link href="/css/b.css">"#;
        let result = rewrite_html_asset_urls(html, "https://proxy.example.com");
        assert!(result.contains("https://proxy.example.com/js/a.js"));
        assert!(result.contains("https://proxy.example.com/css/b.css"));
    }

    #[test]
    fn rewrite_strips_trailing_slash_from_proxy() {
        let html = r#"<script src="/app.js"></script>"#;
        let result = rewrite_html_asset_urls(html, "https://proxy.example.com");
        assert!(result.contains("https://proxy.example.com/app.js"));
        assert!(!result.contains("https://proxy.example.com//app.js"));
    }
}
