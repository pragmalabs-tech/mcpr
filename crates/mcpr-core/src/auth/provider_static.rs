//! Static auth provider: serves operator-configured metadata verbatim.

use std::sync::Arc;

use super::provider::{AuthProvider, OAuthResponse};
use super::types::{OAuthEndpoint, OAuthRequest, ProtectedResourceMetadata};

/// Default provider used when the operator declares
/// `[auth] authorization_servers = [...]` in mcpr.toml. Serves the
/// configured metadata at the protected-resource discovery path and
/// forwards every other OAuth path to upstream.
pub struct StaticAuthProvider {
    metadata: ProtectedResourceMetadata,
}

impl StaticAuthProvider {
    pub fn new(metadata: ProtectedResourceMetadata) -> Arc<Self> {
        Arc::new(Self { metadata })
    }
}

impl AuthProvider for StaticAuthProvider {
    fn handle(&self, req: &OAuthRequest) -> OAuthResponse {
        match req.endpoint {
            OAuthEndpoint::ProtectedResourceMetadata => {
                OAuthResponse::Serve(serde_json::to_value(&self.metadata).unwrap_or_default())
            }
            _ => OAuthResponse::Forward,
        }
    }
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;
    use crate::protocol::http_request::HttpRequest;

    fn sample_metadata() -> ProtectedResourceMetadata {
        ProtectedResourceMetadata {
            resource: "http://localhost:3000".into(),
            authorization_servers: vec!["https://auth.example.com".into()],
            bearer_methods_supported: vec!["header".into()],
            scopes_supported: Some(vec!["mcp:tools".into()]),
            resource_documentation: None,
        }
    }

    fn oauth_req(endpoint: OAuthEndpoint) -> OAuthRequest {
        let http: HttpRequest = http::Request::builder()
            .method("GET")
            .uri("/.well-known/oauth-protected-resource")
            .body(bytes::Bytes::new())
            .unwrap();
        OAuthRequest { endpoint, http }
    }

    #[test]
    fn handle__protected_resource_metadata_serves_configured_doc() {
        let provider = StaticAuthProvider::new(sample_metadata());
        let resp = provider.handle(&oauth_req(OAuthEndpoint::ProtectedResourceMetadata));
        let OAuthResponse::Serve(body) = resp else {
            panic!("expected Serve");
        };
        assert_eq!(body["resource"], "http://localhost:3000");
        assert_eq!(body["authorization_servers"][0], "https://auth.example.com");
    }

    #[test]
    fn handle__non_protected_resource_paths_forward() {
        let provider = StaticAuthProvider::new(sample_metadata());
        for endpoint in [
            OAuthEndpoint::AuthorizationServerMetadata,
            OAuthEndpoint::OpenIdConfiguration,
            OAuthEndpoint::Jwks,
        ] {
            assert!(matches!(
                provider.handle(&oauth_req(endpoint)),
                OAuthResponse::Forward
            ));
        }
    }
}
