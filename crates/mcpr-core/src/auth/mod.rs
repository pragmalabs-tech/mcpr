//! Auth observation + OAuth 2.1 protected-resource surface.
//!
//! - [`types`]: data model for parsed credentials and RFC 9728 metadata.
//! - [`parse`]: parsers invoked from the proxy pipeline (observation only).
//! - [`provider`]: the [`AuthProvider`] trait and [`OAuthResponse`] reply.
//! - [`provider_static`]: serves operator-supplied metadata.
//! - [`provider_reflect`]: probes upstream's well-known URL and serves
//!   the cached metadata.
//!
//! Phase 2 will add concrete validating providers (`JwksProvider`,
//! `Auth0Provider`, `SupabaseProvider`) by implementing the same trait.

pub mod parse;
pub mod provider;
pub mod provider_reflect;
pub mod provider_static;
pub mod types;

pub use parse::{parse_request_auth, parse_www_authenticate};
pub use provider::{AuthProvider, OAuthResponse};
pub use provider_reflect::{ReflectAuthProvider, discovery_url_for};
pub use provider_static::StaticAuthProvider;
pub use types::{
    Audience, AuthRequest, JwtClaims, JwtCredential, JwtHeader, OAuthEndpoint, OAuthRequest,
    ProtectedResourceMetadata, WwwAuthenticateChallenge,
};
