//! `WidgetOverlayMiddleware` — substitute upstream-returned widget HTML with a local
//! bundle for `ui://widget/*` resources. Runs only when:
//!
//! * the proxy has a widget source configured,
//! * the request is a non-batch `resources/read`, and
//! * the requested URI matches `ui://widget/<name>(.html)?`.
//!
//! The upstream response is still issued (handles auth, meta, CSP). This middleware
//! only swaps the `text` field of each matching `contents[]` entry.

use crate::protocol::McpMethod;
use async_trait::async_trait;
use serde_json::Value;

use super::ResponseMiddleware;
use crate::proxy::pipeline::context::{RequestContext, ResponseContext};
use crate::proxy::proxy_state::ProxyState;
use crate::proxy::widgets::fetch_widget_html;

pub struct WidgetOverlayMiddleware;

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use axum::http::{HeaderMap, Method};
    use serde_json::json;
    use tokio::sync::RwLock;

    use super::*;
    use crate::protocol::schema_manager::{MemorySchemaStore, SchemaManager};
    use crate::protocol::session::MemorySessionStore;
    use crate::proxy::forwarding::UpstreamClient;
    use crate::proxy::widgets::WidgetSource;
    use crate::proxy::{CspConfig, RewriteConfig, new_shared_health};

    fn proxy_with_widgets(dir: &std::path::Path) -> ProxyState {
        ProxyState {
            name: "t".into(),
            mcp_upstream: "http://u".into(),
            upstream: UpstreamClient {
                http_client: reqwest::Client::builder().build().unwrap(),
                semaphore: Arc::new(tokio::sync::Semaphore::new(1)),
                request_timeout: Duration::from_secs(1),
            },
            max_request_body: 1024,
            max_response_body: 1024,
            rewrite_config: Arc::new(RwLock::new(RewriteConfig {
                proxy_url: "http://p".into(),
                proxy_domain: "p".into(),
                mcp_upstream: "http://u".into(),
                csp: CspConfig::default(),
            })),
            widget_source: Some(WidgetSource::Static(dir.to_string_lossy().to_string())),
            sessions: MemorySessionStore::new(),
            schema_manager: Arc::new(SchemaManager::new("t", MemorySchemaStore::new())),
            health: new_shared_health(),
            event_bus: crate::event::EventManager::new().start().bus,
        }
    }

    fn batch_ctx_for_widget() -> RequestContext {
        // Build a real batch ParsedBody so `is_batch` + `jsonrpc` stay
        // consistent. The URI is what would otherwise match, proving that
        // the batch flag alone gates the overlay.
        let body = br#"[{"jsonrpc":"2.0","id":1,"method":"resources/read","params":{"uri":"ui://widget/question"}}]"#;
        let parsed = crate::protocol::parse_body(body).unwrap();
        assert!(parsed.is_batch);
        RequestContext {
            start: Instant::now(),
            http_method: Method::POST,
            path: "/mcp".into(),
            request_size: body.len(),
            wants_sse: false,
            session_id: None,
            jsonrpc: Some(parsed),
            mcp_method: Some(McpMethod::ResourcesRead),
            mcp_method_str: Some("resources/read".into()),
            tool: None,
            is_batch: true,
            client_info_from_init: None,
            client_name: None,
            client_version: None,
            tags: Vec::new(),
        }
    }

    #[tokio::test]
    async fn widget_overlay__batch_request_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let wdir = dir.path().join("src/question");
        std::fs::create_dir_all(&wdir).unwrap();
        std::fs::write(wdir.join("index.html"), "LOCAL").unwrap();

        let state = proxy_with_widgets(dir.path());
        let req = batch_ctx_for_widget();
        let mut resp = ResponseContext::new(200, HeaderMap::new(), vec![], None);
        resp.json = Some(json!({
            "result": {"contents": [{"uri": "ui://widget/question", "text": "UPSTREAM"}]}
        }));

        WidgetOverlayMiddleware
            .on_response(&state, &req, &mut resp)
            .await;

        let text = resp.json.as_ref().unwrap()["result"]["contents"][0]["text"]
            .as_str()
            .unwrap();
        assert_eq!(text, "UPSTREAM", "batch requests must skip overlay");
    }
}

#[async_trait]
impl ResponseMiddleware for WidgetOverlayMiddleware {
    async fn on_response(
        &self,
        state: &ProxyState,
        req: &RequestContext,
        resp: &mut ResponseContext,
    ) {
        if state.widget_source.is_none()
            || req.is_batch
            || req.mcp_method != Some(McpMethod::ResourcesRead)
        {
            return;
        }
        let Some(uri) = req
            .jsonrpc
            .as_ref()
            .and_then(|p| p.first_params())
            .and_then(|params| params.get("uri"))
            .and_then(|u| u.as_str())
            .and_then(|u| u.strip_prefix("ui://widget/"))
            .map(|s| s.trim_end_matches(".html").to_string())
        else {
            return;
        };

        let Some(html) = fetch_widget_html(state, &uri).await else {
            return;
        };

        let Some(json) = resp.json.as_mut() else {
            return;
        };
        if let Some(contents) = json
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
    }
}
