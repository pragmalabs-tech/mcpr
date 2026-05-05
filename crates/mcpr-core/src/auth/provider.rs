//! Auth provider trait — the proxy's runtime view of how to respond
//! to an OAuth-classified request.
//!
//! v1 ships two concrete impls in sibling modules:
//! - [`super::StaticAuthProvider`] - serves operator-supplied metadata.
//! - [`super::ReflectAuthProvider`] - probes the upstream MCP origin
//!   at startup and serves the cached metadata.
//!
//! Phase 2 adds signature-verifying providers (`JwksProvider`,
//! `Auth0Provider`, `SupabaseProvider`) by implementing the same trait.

use super::types::{AuthRequest, OAuthRequest};

/// How a provider responds to an [`OAuthRequest`].
///
/// `Serve` short-circuits forwarding; `Forward` falls through to the
/// HTTP forwarder; `NotFound` returns 404 inline.
#[non_exhaustive]
pub enum OAuthResponse {
    /// Provider produced the body inline. Served as 200 OK with
    /// `Content-Type: application/json`.
    Serve(serde_json::Value),
    /// Forward to upstream as if it were a regular HTTP request.
    Forward,
    /// Provider explicitly doesn't serve this endpoint; respond 404.
    NotFound,
}

/// Plug-in surface for OAuth 2.1 providers.
pub trait AuthProvider: Send + Sync {
    /// Decide how to respond to an OAuth-classified request.
    fn handle(&self, req: &OAuthRequest) -> OAuthResponse;

    /// Default: extract `sub` from a parsed JWT. Concrete providers
    /// can override to introspect opaque tokens or cross-reference
    /// an internal store.
    fn user_id(&self, auth: &AuthRequest) -> Option<String> {
        auth.jwt.as_ref().and_then(|j| j.claims.sub.clone())
    }
}
