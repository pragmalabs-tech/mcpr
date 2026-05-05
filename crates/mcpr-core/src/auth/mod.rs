//! Auth observation + OAuth 2.1 protected-resource surface.
//!
//! - [`types`]: data model for parsed credentials and RFC 9728 metadata.
//! - [`parse`]: parsers invoked from the proxy pipeline (observation only).
//! - [`provider`]: the [`AuthProvider`] runtime view of the configured
//!   protected resource. Phase 2 turns this into a trait when concrete
//!   integrations (`JwksProvider`, `Auth0Provider`, `SupabaseProvider`)
//!   land; v1 has one impl so the abstraction is held back.
//! - [`discovery`]: HTTP handler that serves the protected-resource
//!   metadata document at `/.well-known/oauth-protected-resource`.
//!
//! Observation only at this layer: nothing validates signatures,
//! enforces policy, or rewrites traffic.

pub mod discovery;
pub mod parse;
pub mod provider;
pub mod types;

pub use discovery::protected_resource_metadata_handler;
pub use parse::{parse_request_auth, parse_www_authenticate};
pub use provider::AuthProvider;
pub use types::{
    Audience, AuthRequest, JwtClaims, JwtCredential, JwtHeader, ProtectedResourceMetadata,
    WwwAuthenticateChallenge,
};
