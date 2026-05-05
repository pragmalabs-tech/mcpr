//! HTTP handler that serves RFC 9728 protected-resource metadata.
//!
//! Mounted at `/.well-known/oauth-protected-resource` only when the
//! proxy has an [`AuthProvider`] configured. Without `[auth]` in the
//! TOML, the path falls through to the upstream MCP server like any
//! other URL.

use std::sync::Arc;

use axum::{Json, extract::State, http::StatusCode, response::IntoResponse};

use super::AuthProvider;

/// Axum handler for `GET /.well-known/oauth-protected-resource`.
pub async fn protected_resource_metadata_handler(
    State(provider): State<Arc<AuthProvider>>,
) -> impl IntoResponse {
    (StatusCode::OK, Json(provider.protected_resource_metadata()))
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;
    use axum::{Router, body::Body, http::Request, routing::get};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use crate::auth::types::ProtectedResourceMetadata;

    fn sample_metadata() -> ProtectedResourceMetadata {
        ProtectedResourceMetadata {
            resource: "http://localhost:3000".into(),
            authorization_servers: vec!["https://auth.example.com".into()],
            bearer_methods_supported: vec!["header".into()],
            scopes_supported: Some(vec!["mcp:tools".into(), "mcp:resources".into()]),
            resource_documentation: None,
        }
    }

    fn router_with_provider(metadata: ProtectedResourceMetadata) -> Router {
        Router::new()
            .route(
                "/.well-known/oauth-protected-resource",
                get(protected_resource_metadata_handler),
            )
            .with_state(AuthProvider::new(metadata))
    }

    #[tokio::test]
    async fn discovery_handler__returns_metadata_json() {
        let app = router_with_provider(sample_metadata());
        let req = Request::builder()
            .uri("/.well-known/oauth-protected-resource")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["resource"], "http://localhost:3000");
        assert_eq!(v["authorization_servers"][0], "https://auth.example.com");
        assert_eq!(v["bearer_methods_supported"][0], "header");
        assert_eq!(v["scopes_supported"][0], "mcp:tools");
    }

    #[tokio::test]
    async fn discovery_handler__omits_optional_fields_when_none() {
        let app = router_with_provider(ProtectedResourceMetadata {
            resource: "http://localhost:3000".into(),
            authorization_servers: vec!["https://auth.example.com".into()],
            bearer_methods_supported: vec!["header".into()],
            scopes_supported: None,
            resource_documentation: None,
        });

        let req = Request::builder()
            .uri("/.well-known/oauth-protected-resource")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(v.get("scopes_supported").is_none());
        assert!(v.get("resource_documentation").is_none());
    }
}
