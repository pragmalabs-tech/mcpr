//! Header and URI parsers for [`super::types`].
//!
//! All parsing is total. Malformed input degrades to a less-specific
//! variant (`Opaque`, `Custom`, `None`); nothing here returns an error.

use std::sync::OnceLock;

use http::HeaderMap;
use jsonwebtoken::{DecodingKey, Validation, decode, decode_header};
use sha2::{Digest, Sha256};

use super::types::{AuthRequest, JwtClaims, JwtCredential, JwtHeader, WwwAuthenticateChallenge};

/// Inspect the inbound `Authorization` header.
///
/// Returns [`AuthRequest::default`] when the header is absent or its
/// bytes are not valid UTF-8.
pub fn parse_request_auth(headers: &HeaderMap) -> AuthRequest {
    let Some(raw) = headers.get(http::header::AUTHORIZATION) else {
        return AuthRequest::default();
    };
    let Ok(s) = raw.to_str() else {
        return AuthRequest::default();
    };

    let s = s.trim();
    let (scheme, rest) = match s.find(char::is_whitespace) {
        Some(idx) => (&s[..idx], s[idx..].trim_start()),
        None => (s, ""),
    };

    if scheme.is_empty() {
        return AuthRequest::default();
    }

    let jwt = scheme
        .eq_ignore_ascii_case("bearer")
        .then(|| parse_bearer(rest))
        .flatten()
        .map(Box::new);

    let fingerprint = (!rest.is_empty()).then(|| compute_fingerprint(rest.as_bytes()));

    AuthRequest {
        scheme: Some(scheme.to_string()),
        header_name: Some("Authorization".to_string()),
        fingerprint,
        jwt,
    }
}

/// Decode a Bearer token without verifying its signature.
///
/// Observation only: this function ALWAYS disables signature
/// verification and exp/nbf/aud checks via `Validation`. Phase 2
/// providers re-enable those flags through their own decoding paths;
/// this path is for "what does the token claim, regardless of trust".
/// Returns `None` for opaque (non-JWT) bearers.
fn parse_bearer(token: &str) -> Option<JwtCredential> {
    let jwt_header = decode_header(token).ok()?;

    let mut validation = Validation::new(jwt_header.alg);
    // Observation-only: skip every check jsonwebtoken would otherwise run.
    validation.insecure_disable_signature_validation();
    validation.validate_exp = false;
    validation.validate_nbf = false;
    validation.validate_aud = false;
    validation.required_spec_claims.clear();

    let key = DecodingKey::from_secret(b"");
    // `JwtClaims` deserializes JWT payloads directly. The `scope`
    // field accepts string-or-array via a custom `deserialize_with`
    // and the `scopes` alternate field name via `serde(alias)`, so no
    // intermediate struct is needed.
    let data = decode::<JwtClaims>(token, &key, &validation).ok()?;

    Some(JwtCredential {
        header: JwtHeader {
            alg: Some(format!("{:?}", jwt_header.alg)),
            kid: jwt_header.kid,
            typ: jwt_header.typ,
        },
        claims: data.claims,
    })
}

fn process_salt() -> &'static [u8; 16] {
    static SALT: OnceLock<[u8; 16]> = OnceLock::new();
    SALT.get_or_init(|| *uuid::Uuid::new_v4().as_bytes())
}

/// Salted SHA-256 of the credential bytes, truncated to 16 hex chars.
/// Process-stable correlation handle for "same token across requests"
/// without exposing the token. Salt is per-process random so
/// fingerprints are not stable across restarts by design.
fn compute_fingerprint(token: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(process_salt());
    hasher.update(token);
    let digest = hasher.finalize();
    digest.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

// ────────── WWW-Authenticate ──────────

/// Inspect the response `WWW-Authenticate` header.
///
/// Multiple challenge headers are folded by taking the first; multiple
/// challenges packed into one header are not split (everything after
/// the leading scheme is parsed as auth-params for that one challenge).
pub fn parse_www_authenticate(headers: &HeaderMap) -> Option<WwwAuthenticateChallenge> {
    let raw = headers.get("www-authenticate")?.to_str().ok()?;
    let trimmed = raw.trim();

    let (scheme, rest) = match trimmed.find(char::is_whitespace) {
        Some(idx) => (&trimmed[..idx], trimmed[idx..].trim_start()),
        None => (trimmed, ""),
    };
    if scheme.is_empty() {
        return None;
    }

    let mut challenge = WwwAuthenticateChallenge {
        scheme: scheme.to_string(),
        realm: None,
        resource_metadata: None,
        scope: None,
        error: None,
        error_description: None,
    };

    for (key, value) in parse_challenge_params(rest) {
        match key.as_str() {
            "realm" => challenge.realm = Some(value),
            "resource_metadata" => challenge.resource_metadata = Some(value),
            "scope" => {
                let scopes: Vec<String> = value.split_whitespace().map(String::from).collect();
                if !scopes.is_empty() {
                    challenge.scope = Some(scopes);
                }
            }
            "error" => challenge.error = Some(value),
            "error_description" => challenge.error_description = Some(value),
            _ => {}
        }
    }

    Some(challenge)
}

/// Parse RFC 7235 auth-param list. Tolerant of whitespace and missing
/// values; quoted-strings honour backslash escapes.
fn parse_challenge_params(input: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut chars = input.chars().peekable();

    loop {
        while matches!(chars.peek(), Some(c) if c.is_whitespace() || *c == ',') {
            chars.next();
        }

        let mut key = String::new();
        while let Some(&c) = chars.peek() {
            if c.is_alphanumeric() || c == '_' || c == '-' {
                key.push(c);
                chars.next();
            } else {
                break;
            }
        }
        if key.is_empty() {
            break;
        }

        while matches!(chars.peek(), Some(c) if c.is_whitespace()) {
            chars.next();
        }
        if chars.peek() != Some(&'=') {
            out.push((key, String::new()));
            continue;
        }
        chars.next();
        while matches!(chars.peek(), Some(c) if c.is_whitespace()) {
            chars.next();
        }

        let value = if chars.peek() == Some(&'"') {
            chars.next();
            let mut v = String::new();
            while let Some(c) = chars.next() {
                match c {
                    '\\' => {
                        if let Some(escaped) = chars.next() {
                            v.push(escaped);
                        }
                    }
                    '"' => break,
                    other => v.push(other),
                }
            }
            v
        } else {
            let mut v = String::new();
            while let Some(&c) = chars.peek() {
                if c == ',' || c.is_whitespace() {
                    break;
                }
                v.push(c);
                chars.next();
            }
            v
        };

        out.push((key.to_ascii_lowercase(), value));
    }

    out
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;
    use crate::auth::types::Audience;
    use base64::Engine;
    use serde_json::json;

    /// Unwrap an [`AuthRequest`] as a parsed JWT. Panics otherwise.
    fn expect_jwt(out: AuthRequest) -> JwtCredential {
        *out.jwt.expect("expected JWT credential")
    }

    /// Test whether the request observed a Bearer with no decodable JWT.
    fn is_bearer_opaque(out: &AuthRequest) -> bool {
        out.scheme
            .as_deref()
            .is_some_and(|s| s.eq_ignore_ascii_case("bearer"))
            && out.jwt.is_none()
    }

    fn headers_authorization(value: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(http::header::AUTHORIZATION, value.parse().unwrap());
        h
    }

    fn headers_www_authenticate(value: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert("www-authenticate", value.parse().unwrap());
        h
    }

    fn jwt(header: serde_json::Value, claims: serde_json::Value, sig: &str) -> String {
        let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let h = engine.encode(serde_json::to_string(&header).unwrap());
        let c = engine.encode(serde_json::to_string(&claims).unwrap());
        format!("{h}.{c}.{sig}")
    }

    // ── parse_request_auth ────────────────────────────────────

    #[test]
    fn parse_request_auth__no_header_returns_default() {
        let out = parse_request_auth(&HeaderMap::new());
        assert!(out.is_none());
        assert!(out.fingerprint.is_none());
    }

    #[test]
    fn parse_request_auth__bearer_jwt_full_claims() {
        let token = jwt(
            json!({"alg": "RS256", "kid": "k1", "typ": "JWT"}),
            json!({
                "iss": "https://auth.example.com",
                "sub": "user@example.com",
                "aud": "mcpr",
                "exp": 1735689600_i64,
                "iat": 1735603200_i64,
                "scope": "read write admin",
                "client_id": "client-abc",
            }),
            "sig",
        );
        let out = parse_request_auth(&headers_authorization(&format!("Bearer {token}")));
        let c = expect_jwt(out);
        assert_eq!(c.header.alg.as_deref(), Some("RS256"));
        assert_eq!(c.header.kid.as_deref(), Some("k1"));
        assert_eq!(c.claims.iss.as_deref(), Some("https://auth.example.com"));
        assert_eq!(c.claims.sub.as_deref(), Some("user@example.com"));
        assert!(matches!(c.claims.aud, Some(Audience::Single(ref s)) if s == "mcpr"));
        assert_eq!(c.claims.exp, Some(1735689600));
        assert_eq!(
            c.claims.scope.as_deref(),
            Some(["read", "write", "admin"].map(String::from).as_slice())
        );
        assert_eq!(c.claims.client_id.as_deref(), Some("client-abc"));
    }

    #[test]
    fn parse_request_auth__jwt_aud_array_yields_multi() {
        let token = jwt(
            json!({"alg": "HS256"}),
            json!({"aud": ["mcpr", "service-b"]}),
            "sig",
        );
        let out = parse_request_auth(&headers_authorization(&format!("Bearer {token}")));
        let c = expect_jwt(out);
        assert!(matches!(c.claims.aud, Some(Audience::Multi(ref v)) if v.len() == 2));
    }

    #[test]
    fn parse_request_auth__jwt_scopes_array_field() {
        let token = jwt(
            json!({"alg": "HS256"}),
            json!({"scopes": ["a", "b"]}),
            "sig",
        );
        let out = parse_request_auth(&headers_authorization(&format!("Bearer {token}")));
        let c = expect_jwt(out);
        assert_eq!(
            c.claims.scope.as_deref(),
            Some(["a", "b"].map(String::from).as_slice())
        );
    }

    #[test]
    fn parse_request_auth__jwt_extra_claims_captured() {
        let token = jwt(
            json!({"alg": "HS256"}),
            json!({"sub": "x", "custom": "y", "tenant": 7}),
            "sig",
        );
        let out = parse_request_auth(&headers_authorization(&format!("Bearer {token}")));
        let c = expect_jwt(out);
        assert_eq!(
            c.claims.extra.get("custom").and_then(|v| v.as_str()),
            Some("y")
        );
        assert_eq!(
            c.claims.extra.get("tenant").and_then(|v| v.as_i64()),
            Some(7)
        );
        assert!(!c.claims.extra.contains_key("sub"));
    }

    #[test]
    fn parse_request_auth__bearer_opaque_when_not_three_parts() {
        let out = parse_request_auth(&headers_authorization("Bearer not-a-jwt"));
        assert!(is_bearer_opaque(&out));
        assert!(out.fingerprint.is_some());
    }

    #[test]
    fn parse_request_auth__bearer_opaque_when_payload_not_base64() {
        let out = parse_request_auth(&headers_authorization("Bearer aaa.!!!.ccc"));
        assert!(is_bearer_opaque(&out));
    }

    #[test]
    fn parse_request_auth__bearer_opaque_when_payload_not_json() {
        let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let token = format!(
            "{}.{}.sig",
            engine.encode("notjson"),
            engine.encode("alsonot")
        );
        let out = parse_request_auth(&headers_authorization(&format!("Bearer {token}")));
        assert!(is_bearer_opaque(&out));
    }

    #[test]
    fn parse_request_auth__bearer_alg_none_classifies_as_opaque() {
        // jsonwebtoken refuses `alg: "none"` (the unsigned-JWT footgun)
        // by default, so we record it as opaque rather than parsed JWT.
        // Clients should never send these and we don't need to decode them.
        let token = jwt(json!({"alg": "none"}), json!({"sub": "x"}), "");
        let out = parse_request_auth(&headers_authorization(&format!("Bearer {token}")));
        assert!(is_bearer_opaque(&out));
    }

    #[test]
    fn parse_request_auth__custom_scheme_recorded() {
        let out = parse_request_auth(&headers_authorization("Token foo-bar"));
        assert_eq!(out.scheme.as_deref(), Some("Token"));
        assert!(out.jwt.is_none());
    }

    #[test]
    fn parse_request_auth__basic_records_scheme_only() {
        let payload = base64::engine::general_purpose::STANDARD.encode("alice:secret123");
        let out = parse_request_auth(&headers_authorization(&format!("Basic {payload}")));
        assert_eq!(out.scheme.as_deref(), Some("Basic"));
        assert!(out.jwt.is_none());
        assert!(out.fingerprint.is_some());
    }

    #[test]
    fn parse_request_auth__scheme_is_case_insensitive_for_bearer_decode() {
        let out = parse_request_auth(&headers_authorization("bEaReR aaa.bbb.ccc"));
        assert_eq!(out.scheme.as_deref(), Some("bEaReR"));
        // The scheme name is preserved as-written, but the bearer-detection
        // path runs case-insensitively so opaque-classification still kicks in.
        assert!(out.jwt.is_none()); // not a real JWT, decodes as opaque
    }

    #[test]
    fn parse_request_auth__records_header_name() {
        let out = parse_request_auth(&headers_authorization("Bearer x"));
        assert_eq!(out.header_name.as_deref(), Some("Authorization"));
    }

    // ── compute_fingerprint ───────────────────────────────────

    #[test]
    fn compute_fingerprint__same_token_stable_in_process() {
        let a = compute_fingerprint(b"the-same-token");
        let b = compute_fingerprint(b"the-same-token");
        assert_eq!(a, b);
    }

    #[test]
    fn compute_fingerprint__different_tokens_differ() {
        let a = compute_fingerprint(b"token-one");
        let b = compute_fingerprint(b"token-two");
        assert_ne!(a, b);
    }

    #[test]
    fn compute_fingerprint__has_expected_shape() {
        let fp = compute_fingerprint(b"x");
        assert_eq!(fp.len(), 16);
        assert!(fp.chars().all(|c| c.is_ascii_hexdigit()));
    }

    // ── parse_www_authenticate ────────────────────────────────

    #[test]
    fn parse_www_authenticate__absent_returns_none() {
        assert!(parse_www_authenticate(&HeaderMap::new()).is_none());
    }

    #[test]
    fn parse_www_authenticate__bearer_with_full_params() {
        let h = headers_www_authenticate(
            r#"Bearer realm="mcpr", resource_metadata="https://auth.example.com/.well-known/oauth-protected-resource", error="invalid_token", error_description="The access token expired", scope="read write""#,
        );
        let c = parse_www_authenticate(&h).unwrap();
        assert_eq!(c.scheme, "Bearer");
        assert_eq!(c.realm.as_deref(), Some("mcpr"));
        assert_eq!(
            c.resource_metadata.as_deref(),
            Some("https://auth.example.com/.well-known/oauth-protected-resource")
        );
        assert_eq!(c.error.as_deref(), Some("invalid_token"));
        assert_eq!(
            c.error_description.as_deref(),
            Some("The access token expired")
        );
        assert_eq!(
            c.scope.as_deref(),
            Some(["read", "write"].map(String::from).as_slice())
        );
    }

    #[test]
    fn parse_www_authenticate__scheme_only() {
        let c = parse_www_authenticate(&headers_www_authenticate("Bearer")).unwrap();
        assert_eq!(c.scheme, "Bearer");
        assert!(c.realm.is_none());
    }

    #[test]
    fn parse_www_authenticate__unknown_params_dropped() {
        let c = parse_www_authenticate(&headers_www_authenticate(
            r#"Bearer realm="x", custom_param="ignored""#,
        ))
        .unwrap();
        assert_eq!(c.realm.as_deref(), Some("x"));
    }

    #[test]
    fn parse_www_authenticate__quoted_string_with_escape() {
        let c = parse_www_authenticate(&headers_www_authenticate(
            r#"Bearer realm="my \"quoted\" realm""#,
        ))
        .unwrap();
        assert_eq!(c.realm.as_deref(), Some(r#"my "quoted" realm"#));
    }

    #[test]
    fn parse_www_authenticate__param_keys_case_insensitive() {
        let c = parse_www_authenticate(&headers_www_authenticate(
            r#"Bearer Realm="x", ERROR="invalid_token""#,
        ))
        .unwrap();
        assert_eq!(c.realm.as_deref(), Some("x"));
        assert_eq!(c.error.as_deref(), Some("invalid_token"));
    }
}
