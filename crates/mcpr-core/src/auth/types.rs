//! Auth observation types.
//!
//! Two orthogonal axes captured per request:
//!
//! - [`AuthRequest`] - what credential the inbound request carried
//!   (scheme, parsed JWT claims when bearer, fingerprint).
//! - [`WwwAuthenticateChallenge`] - challenge captured from the
//!   upstream response, useful for clients chasing OAuth discovery.
//!
//! Plus the resource-side surface ([`ProtectedResourceMetadata`])
//! served at `/.well-known/oauth-protected-resource`.
//!
//! Everything is observation only. Nothing here verifies signatures,
//! enforces policy, or mutates traffic.

use serde::{Deserialize, Deserializer, Serialize};

use crate::protocol::http_request::HttpRequest;

/// Captured auth context for a single inbound request.
///
/// Flat by design: every field is independently optional. `scheme` is
/// the only "is auth present" signal; downstream code reads `jwt` /
/// `fingerprint` / `header_name` as needed.
#[derive(Default, Clone, Debug, Serialize, Deserialize)]
#[non_exhaustive]
pub struct AuthRequest {
    /// Auth scheme name observed on the `Authorization` header
    /// ("bearer", "basic", "dpop", "token", ...). Lowercase as written
    /// on the wire (case is preserved verbatim). `None` when no
    /// `Authorization` header was present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scheme: Option<String>,
    /// Header name where the credential was found. Always
    /// "Authorization" today; reserved for future X-Api-Key / Cookie
    /// support without forcing an enum.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub header_name: Option<String>,
    /// Stable per-process correlation handle for the raw token bytes.
    /// 16 hex chars from a salted SHA-256 of the credential value.
    /// `None` when no credential was presented.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    /// JWT decoded without signature verification. `Some` only when
    /// the credential was a Bearer that decoded as a JWT.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jwt: Option<Box<JwtCredential>>,
}

impl AuthRequest {
    /// True when no credential was observed on the wire.
    pub fn is_none(&self) -> bool {
        self.scheme.is_none()
    }
}

/// JWT decoded without signature verification.
#[derive(Default, Clone, Debug, Serialize, Deserialize)]
#[non_exhaustive]
pub struct JwtCredential {
    pub header: JwtHeader,
    pub claims: JwtClaims,
}

#[derive(Default, Clone, Debug, Serialize, Deserialize)]
#[non_exhaustive]
pub struct JwtHeader {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alg: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub typ: Option<String>,
}

#[derive(Default, Clone, Debug, Serialize, Deserialize)]
#[non_exhaustive]
pub struct JwtClaims {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iss: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sub: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aud: Option<Audience>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exp: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iat: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nbf: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jti: Option<String>,
    /// Always emitted as a JSON array of scope strings. On input,
    /// accepts either the RFC 9068 space-separated string form
    /// (`"scope": "read write"`) or a JSON array (`"scope": ["read",
    /// "write"]`), and the alternate field name `scopes` some providers
    /// emit.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_scope",
        alias = "scopes"
    )]
    pub scope: Option<Vec<String>>,
    /// RFC 9068 `client_id` claim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    /// Anything not in the named-claim set lives here. `flatten` captures
    /// unknown JWT claims directly into the map on deserialize.
    #[serde(flatten, skip_serializing_if = "serde_json::Map::is_empty")]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// JWT `aud` claim, which is either a single string or an array.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
#[non_exhaustive]
pub enum Audience {
    Single(String),
    Multi(Vec<String>),
}

/// Deserialize the JWT `scope` claim from either the RFC 9068
/// space-separated string form or a JSON array. Empty results
/// degrade to `None` so consumers don't have to distinguish
/// "absent" from "empty".
fn deserialize_scope<'de, D>(deserializer: D) -> Result<Option<Vec<String>>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    let scopes = match value {
        None | Some(serde_json::Value::Null) => return Ok(None),
        Some(serde_json::Value::String(s)) => {
            s.split_whitespace().map(String::from).collect::<Vec<_>>()
        }
        Some(serde_json::Value::Array(arr)) => arr
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect(),
        // Other JSON shapes (number, bool, object) for `scope` are
        // malformed per spec; ignore rather than error so observation
        // stays total.
        _ => return Ok(None),
    };
    Ok((!scopes.is_empty()).then_some(scopes))
}

// ────────── Inbound OAuth request classification ──────────

/// Inbound request classified as an OAuth protocol call by URL path.
/// Carries the raw [`HttpRequest`] so forwarding stays identical to
/// regular HTTP traffic when the provider returns
/// [`OAuthResponse::Forward`].
#[derive(Clone, Debug)]
pub struct OAuthRequest {
    pub endpoint: OAuthEndpoint,
    pub http: HttpRequest,
}

/// Well-known OAuth endpoint paths an MCP-fronting proxy might see.
/// Tight set: only the discovery paths a protected resource (or its
/// co-located authorization server) typically exposes. `/token`,
/// `/authorize`, `/register` etc. live on the auth server, not on
/// the proxy, and are deliberately not classified.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum OAuthEndpoint {
    /// `/.well-known/oauth-protected-resource` (RFC 9728).
    ProtectedResourceMetadata,
    /// `/.well-known/oauth-authorization-server` (RFC 8414).
    AuthorizationServerMetadata,
    /// `/.well-known/openid-configuration` (OIDC discovery).
    OpenIdConfiguration,
    /// `/.well-known/jwks.json`.
    Jwks,
}

impl OAuthEndpoint {
    /// Classify a URI path against well-known OAuth paths. Returns
    /// `None` for paths outside the recognised set.
    pub fn from_path(path: &str) -> Option<Self> {
        match path {
            "/.well-known/oauth-protected-resource" => Some(Self::ProtectedResourceMetadata),
            "/.well-known/oauth-authorization-server" => Some(Self::AuthorizationServerMetadata),
            "/.well-known/openid-configuration" => Some(Self::OpenIdConfiguration),
            "/.well-known/jwks.json" => Some(Self::Jwks),
            _ => None,
        }
    }
}

// ────────── OAuth 2.1 protected resource surface ──────────

/// RFC 9728 OAuth 2.0 Protected Resource Metadata. Served at
/// `/.well-known/oauth-protected-resource`. Tells MCP clients which
/// authorization server(s) to use, what bearer methods are accepted,
/// and which scopes are available.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ProtectedResourceMetadata {
    /// Canonical URL of this protected resource.
    pub resource: String,
    /// One or more issuers the resource trusts.
    pub authorization_servers: Vec<String>,
    /// Default `["header"]`. Other valid: `body`, `query`.
    pub bearer_methods_supported: Vec<String>,
    /// Optional list of scope values the resource recognises.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scopes_supported: Option<Vec<String>>,
    /// Optional human-readable doc URL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource_documentation: Option<String>,
}

/// `WWW-Authenticate` challenge captured from the upstream response.
/// MCP spec 2025-06-18 says auth servers SHOULD include the
/// `resource_metadata` parameter pointing clients at RFC 9728 discovery.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[non_exhaustive]
pub struct WwwAuthenticateChallenge {
    pub scheme: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub realm: Option<String>,
    /// MCP / RFC 9728 hint to the protected resource metadata document.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource_metadata: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<Vec<String>>,
    /// `invalid_token`, `insufficient_scope`, etc.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_description: Option<String>,
}
