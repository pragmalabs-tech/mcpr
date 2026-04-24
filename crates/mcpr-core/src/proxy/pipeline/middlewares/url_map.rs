//! Response-side middleware: rewrite upstream base URLs to the proxy
//! URL in OAuth discovery / JSON passthrough bodies.
//!
//! `Response::Raw` rewriting is gated on a JSON content-type header.
//! Non-JSON `Raw` responses stream through untouched.

use std::sync::Arc;

use arc_swap::ArcSwap;
use async_trait::async_trait;
use axum::body::{Body, Bytes};
use axum::http::HeaderMap;
use axum::http::header::CONTENT_TYPE;

use crate::proxy::RewriteConfig;
use crate::proxy::pipeline::middleware::ResponseMiddleware;
use crate::proxy::pipeline::values::{Context, Response};
use crate::proxy::sse::split_upstream;

pub struct UrlMapMiddleware {
    config: Arc<ArcSwap<RewriteConfig>>,
}

impl UrlMapMiddleware {
    pub fn new(config: Arc<ArcSwap<RewriteConfig>>) -> Self {
        Self { config }
    }
}

#[async_trait]
impl ResponseMiddleware for UrlMapMiddleware {
    fn name(&self) -> &'static str {
        "url_map"
    }

    async fn on_response(&self, resp: Response, _cx: &mut Context) -> Response {
        match resp {
            Response::OauthJson {
                doc,
                status,
                headers,
            } => {
                let bytes = serde_json::to_vec(&doc).unwrap_or_default();
                let rewritten = rewrite_bytes(&self.config, Bytes::from(bytes));
                let doc = serde_json::from_slice(&rewritten).unwrap_or(doc);
                Response::OauthJson {
                    doc,
                    status,
                    headers,
                }
            }
            Response::Raw {
                body,
                status,
                headers,
            } if is_json(&headers) => {
                let bytes = axum::body::to_bytes(body, usize::MAX)
                    .await
                    .unwrap_or_default();
                let rewritten = rewrite_bytes(&self.config, bytes);
                Response::Raw {
                    body: Body::from(rewritten),
                    status,
                    headers,
                }
            }
            other => other,
        }
    }
}

fn rewrite_bytes(config: &ArcSwap<RewriteConfig>, body: Bytes) -> Bytes {
    let cfg = config.load();
    let (upstream_base, _) = split_upstream(&cfg.mcp_upstream);
    let upstream_base = upstream_base.trim_end_matches('/');
    let proxy_url = cfg.proxy_url.trim_end_matches('/');

    if !contains_slice(&body, upstream_base.as_bytes()) {
        return body;
    }

    let body_str = String::from_utf8_lossy(&body);
    Bytes::from(body_str.replace(upstream_base, proxy_url).into_bytes())
}

fn is_json(headers: &HeaderMap) -> bool {
    headers
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|ct| ct.contains("json"))
        .unwrap_or(false)
}

fn contains_slice(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || haystack.len() < needle.len() {
        return needle.is_empty();
    }
    haystack.windows(needle.len()).any(|win| win == needle)
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;

    use axum::http::StatusCode;
    use serde_json::json;

    use crate::proxy::pipeline::middlewares::test_support::{test_context, test_proxy_state};

    fn middleware(proxy: &Arc<crate::proxy::ProxyState>) -> UrlMapMiddleware {
        UrlMapMiddleware::new(proxy.rewrite_config.clone())
    }

    #[tokio::test]
    async fn on_response__oauth_rewrites_upstream_to_proxy() {
        let proxy = test_proxy_state();
        let mut cx = test_context(proxy.clone());
        let resp = Response::OauthJson {
            doc: json!({"issuer": "http://upstream.test/auth"}),
            status: StatusCode::OK,
            headers: HeaderMap::new(),
        };

        let out = middleware(&proxy).on_response(resp, &mut cx).await;
        match out {
            Response::OauthJson { doc, .. } => {
                assert_eq!(doc["issuer"].as_str(), Some("https://proxy.test/auth"));
            }
            _ => panic!("expected OauthJson"),
        }
    }

    #[tokio::test]
    async fn on_response__oauth_no_match_is_identity() {
        let proxy = test_proxy_state();
        let mut cx = test_context(proxy.clone());
        let resp = Response::OauthJson {
            doc: json!({"issuer": "http://other.example.com"}),
            status: StatusCode::OK,
            headers: HeaderMap::new(),
        };

        let out = middleware(&proxy).on_response(resp, &mut cx).await;
        match out {
            Response::OauthJson { doc, .. } => {
                assert_eq!(doc["issuer"].as_str(), Some("http://other.example.com"));
            }
            _ => panic!("expected OauthJson"),
        }
    }

    #[tokio::test]
    async fn on_response__raw_json_rewrites() {
        let proxy = test_proxy_state();
        let mut cx = test_context(proxy.clone());
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, "application/json".parse().unwrap());
        let resp = Response::Raw {
            body: Body::from(r#"{"url":"http://upstream.test/path"}"#),
            status: StatusCode::OK,
            headers,
        };

        let out = middleware(&proxy).on_response(resp, &mut cx).await;
        match out {
            Response::Raw { body, .. } => {
                let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
                let s = std::str::from_utf8(&bytes).unwrap();
                assert!(
                    s.contains("https://proxy.test/path"),
                    "expected proxy url in {s}"
                );
            }
            _ => panic!("expected Raw"),
        }
    }

    #[tokio::test]
    async fn on_response__raw_non_json_passthrough() {
        let proxy = test_proxy_state();
        let mut cx = test_context(proxy.clone());
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_TYPE, "text/html".parse().unwrap());
        let resp = Response::Raw {
            body: Body::from("http://upstream.test/"),
            status: StatusCode::OK,
            headers,
        };

        let out = middleware(&proxy).on_response(resp, &mut cx).await;
        match out {
            Response::Raw { body, .. } => {
                let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
                assert_eq!(
                    std::str::from_utf8(&bytes).unwrap(),
                    "http://upstream.test/"
                );
            }
            _ => panic!("expected Raw"),
        }
    }

    #[tokio::test]
    async fn on_response__mcp_buffered_passthrough() {
        let proxy = test_proxy_state();
        let mut cx = test_context(proxy.clone());
        let resp = Response::Upstream502 { reason: "x".into() };

        let out = middleware(&proxy).on_response(resp, &mut cx).await;
        assert!(matches!(out, Response::Upstream502 { .. }));
    }
}
