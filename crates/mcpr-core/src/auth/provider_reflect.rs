//! Reflective auth provider: probes upstream's well-known discovery
//! URL at startup, caches the response, serves it as the proxy's own.
//!
//! Used when the operator declares `[auth]` without explicit
//! `authorization_servers` - mcpr fronts an MCP server that already
//! exposes its protected-resource metadata, so we mirror it.

use std::sync::Arc;

use arc_swap::ArcSwap;
use url::Url;

use super::provider::{AuthProvider, OAuthResponse};
use super::types::{OAuthEndpoint, OAuthRequest, ProtectedResourceMetadata};

/// Mirrors the upstream resource's RFC 9728 metadata.
pub struct ReflectAuthProvider {
    /// Hot-swappable so a future refresh task can replace cached
    /// metadata atomically without locking readers. `None` is reserved
    /// for the brief window between construction and a successful
    /// probe; v1 always probes before construction succeeds, so
    /// readers always see `Some(_)`.
    metadata: ArcSwap<Option<ProtectedResourceMetadata>>,
    /// The URL we probed. A future refresh task re-probes this.
    discovery_url: Url,
}

impl ReflectAuthProvider {
    /// Probe `discovery_url`, parse the response as RFC 9728 metadata,
    /// cache it. Errors when the probe fails or the response isn't
    /// valid metadata; callers degrade gracefully (warn + don't mount).
    pub async fn probe(discovery_url: Url) -> anyhow::Result<Arc<Self>> {
        let resp = reqwest::get(discovery_url.as_str()).await?;
        if !resp.status().is_success() {
            anyhow::bail!(
                "discovery probe {} returned {}",
                discovery_url,
                resp.status()
            );
        }
        let metadata: ProtectedResourceMetadata = resp.json().await?;
        Ok(Arc::new(Self {
            metadata: ArcSwap::from_pointee(Some(metadata)),
            discovery_url,
        }))
    }

    /// URL we probed and would re-probe on refresh.
    pub fn discovery_url(&self) -> &Url {
        &self.discovery_url
    }
}

impl AuthProvider for ReflectAuthProvider {
    fn handle(&self, req: &OAuthRequest) -> OAuthResponse {
        match req.endpoint {
            OAuthEndpoint::ProtectedResourceMetadata => match self.metadata.load().as_ref() {
                Some(m) => OAuthResponse::Serve(serde_json::to_value(m).unwrap_or_default()),
                None => OAuthResponse::Forward,
            },
            // Authorization server metadata, OIDC, JWKS aren't ours to
            // mirror in v1; let upstream answer them.
            _ => OAuthResponse::Forward,
        }
    }
}

/// Compute the discovery probe URL from the upstream MCP URL: keep
/// origin, set path to `/.well-known/oauth-protected-resource`, drop
/// any query.
pub fn discovery_url_for(mcp_url: &Url) -> Url {
    let mut url = mcp_url.clone();
    url.set_path("/.well-known/oauth-protected-resource");
    url.set_query(None);
    url
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;

    #[test]
    fn discovery_url_for__keeps_origin_drops_path_and_query() {
        let mcp: Url = "http://localhost:9001/mcp".parse().unwrap();
        let derived = discovery_url_for(&mcp);
        assert_eq!(
            derived.as_str(),
            "http://localhost:9001/.well-known/oauth-protected-resource"
        );
    }

    #[test]
    fn discovery_url_for__drops_query_string() {
        let mcp: Url = "https://api.example.com/v1/mcp?a=1&b=2".parse().unwrap();
        let derived = discovery_url_for(&mcp);
        assert_eq!(
            derived.as_str(),
            "https://api.example.com/.well-known/oauth-protected-resource"
        );
    }

    #[test]
    fn discovery_url_for__preserves_https_and_port() {
        let mcp: Url = "https://mcp.example.com:8443/v2/mcp".parse().unwrap();
        let derived = discovery_url_for(&mcp);
        assert_eq!(
            derived.as_str(),
            "https://mcp.example.com:8443/.well-known/oauth-protected-resource"
        );
    }

    /// Spawn a tiny axum server returning the supplied JSON for the
    /// well-known path; return its origin URL.
    async fn spawn_discovery(metadata: serde_json::Value) -> Url {
        use axum::{Router, response::Json, routing::get};
        let app = Router::new().route(
            "/.well-known/oauth-protected-resource",
            get(move || async move { Json(metadata.clone()) }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{addr}/.well-known/oauth-protected-resource")
            .parse()
            .unwrap()
    }

    #[tokio::test]
    async fn probe__returns_provider_with_cached_metadata() {
        let url = spawn_discovery(serde_json::json!({
            "resource": "http://upstream:9001",
            "authorization_servers": ["https://auth.example.com"],
            "bearer_methods_supported": ["header"],
        }))
        .await;

        let provider = ReflectAuthProvider::probe(url).await.unwrap();
        let req = OAuthRequest {
            endpoint: OAuthEndpoint::ProtectedResourceMetadata,
            http: http::Request::builder()
                .uri("/.well-known/oauth-protected-resource")
                .body(bytes::Bytes::new())
                .unwrap(),
        };
        let OAuthResponse::Serve(body) = provider.handle(&req) else {
            panic!("expected Serve");
        };
        assert_eq!(body["resource"], "http://upstream:9001");
        assert_eq!(body["authorization_servers"][0], "https://auth.example.com");
    }

    #[tokio::test]
    async fn probe__errors_on_non_2xx() {
        async fn handler() -> (axum::http::StatusCode, &'static str) {
            (axum::http::StatusCode::NOT_FOUND, "")
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = axum::Router::new().route(
            "/.well-known/oauth-protected-resource",
            axum::routing::get(handler),
        );
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let url: Url = format!("http://{addr}/.well-known/oauth-protected-resource")
            .parse()
            .unwrap();
        assert!(ReflectAuthProvider::probe(url).await.is_err());
    }

    #[tokio::test]
    async fn probe__errors_on_invalid_json() {
        async fn handler() -> &'static str {
            "not json at all"
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = axum::Router::new().route(
            "/.well-known/oauth-protected-resource",
            axum::routing::get(handler),
        );
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let url: Url = format!("http://{addr}/.well-known/oauth-protected-resource")
            .parse()
            .unwrap();
        assert!(ReflectAuthProvider::probe(url).await.is_err());
    }

    #[tokio::test]
    async fn handle__non_protected_resource_paths_forward() {
        let url = spawn_discovery(serde_json::json!({
            "resource": "http://x",
            "authorization_servers": ["https://y"],
            "bearer_methods_supported": ["header"],
        }))
        .await;
        let provider = ReflectAuthProvider::probe(url).await.unwrap();
        for endpoint in [
            OAuthEndpoint::AuthorizationServerMetadata,
            OAuthEndpoint::OpenIdConfiguration,
            OAuthEndpoint::Jwks,
        ] {
            let req = OAuthRequest {
                endpoint,
                http: http::Request::builder()
                    .uri("/")
                    .body(bytes::Bytes::new())
                    .unwrap(),
            };
            assert!(matches!(provider.handle(&req), OAuthResponse::Forward));
        }
    }
}
