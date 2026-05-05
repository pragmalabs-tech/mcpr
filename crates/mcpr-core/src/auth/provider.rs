//! Auth provider - the proxy's runtime view of the configured OAuth
//! protected resource.
//!
//! v1 ships a single concrete struct: it serves the static metadata
//! configured under `[auth]` and extracts a user id from the JWT `sub`
//! claim. When Phase 2 introduces signature-verifying providers
//! (`JwksProvider`, `Auth0Provider`, `SupabaseProvider`), this struct
//! becomes a `trait` with concrete impls.

use std::sync::Arc;

use super::types::{AuthRequest, ProtectedResourceMetadata};

/// Runtime auth provider. Wraps the resource metadata served at
/// `/.well-known/oauth-protected-resource` and exposes a user-identity
/// lookup (`user_id`).
pub struct AuthProvider {
    metadata: ProtectedResourceMetadata,
}

impl AuthProvider {
    pub fn new(metadata: ProtectedResourceMetadata) -> Arc<Self> {
        Arc::new(Self { metadata })
    }

    /// Discovery document advertised at the well-known path.
    pub fn protected_resource_metadata(&self) -> ProtectedResourceMetadata {
        self.metadata.clone()
    }

    /// Extract the user id from the parsed credential. v1 reads JWT
    /// `sub` when present; concrete providers in Phase 2 may override
    /// this path with introspection.
    pub fn user_id(&self, auth: &AuthRequest) -> Option<String> {
        auth.jwt.as_ref().and_then(|j| j.claims.sub.clone())
    }
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;
    use crate::auth::types::{JwtClaims, JwtCredential, JwtHeader};

    fn sample_metadata() -> ProtectedResourceMetadata {
        ProtectedResourceMetadata {
            resource: "http://localhost:3000".into(),
            authorization_servers: vec!["https://auth.example.com".into()],
            bearer_methods_supported: vec!["header".into()],
            scopes_supported: Some(vec!["mcp:tools".into()]),
            resource_documentation: None,
        }
    }

    fn auth_with_jwt_sub(sub: Option<&str>) -> AuthRequest {
        AuthRequest {
            scheme: Some("bearer".into()),
            header_name: Some("Authorization".into()),
            fingerprint: None,
            jwt: Some(Box::new(JwtCredential {
                header: JwtHeader::default(),
                claims: JwtClaims {
                    sub: sub.map(String::from),
                    ..JwtClaims::default()
                },
            })),
        }
    }

    #[test]
    fn protected_resource_metadata__returns_configured_metadata() {
        let provider = AuthProvider::new(sample_metadata());
        let m = provider.protected_resource_metadata();
        assert_eq!(m.resource, "http://localhost:3000");
        assert_eq!(m.authorization_servers, vec!["https://auth.example.com"]);
    }

    #[test]
    fn user_id__extracts_jwt_sub() {
        let provider = AuthProvider::new(sample_metadata());
        let user = provider.user_id(&auth_with_jwt_sub(Some("alice@example.com")));
        assert_eq!(user.as_deref(), Some("alice@example.com"));
    }

    #[test]
    fn user_id__none_when_jwt_has_no_sub() {
        let provider = AuthProvider::new(sample_metadata());
        let user = provider.user_id(&auth_with_jwt_sub(None));
        assert!(user.is_none());
    }

    #[test]
    fn user_id__none_when_no_jwt() {
        let provider = AuthProvider::new(sample_metadata());
        let user = provider.user_id(&AuthRequest::default());
        assert!(user.is_none());
    }

    #[test]
    fn user_id__none_for_non_bearer_scheme() {
        let provider = AuthProvider::new(sample_metadata());
        let auth = AuthRequest {
            scheme: Some("Token".into()),
            ..AuthRequest::default()
        };
        let user = provider.user_id(&auth);
        assert!(user.is_none());
    }
}
