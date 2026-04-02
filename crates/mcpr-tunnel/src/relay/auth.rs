use serde::Deserialize;

/// Configuration for the external auth provider.
pub struct AuthProviderConfig {
    /// Base URL of the auth provider (e.g. "https://auth.mcpr.app")
    pub url: String,
    /// Shared secret for relay ↔ auth provider trust
    pub secret: String,
    /// HTTP client for making verification requests
    pub client: reqwest::Client,
}

/// Response from the auth provider's /api/verify endpoint.
#[derive(Deserialize)]
struct AuthVerifyResponse {
    subdomains: Vec<String>,
}

/// Error response from the auth provider.
#[derive(Deserialize)]
struct AuthErrorResponse {
    error: String,
}

#[derive(Debug)]
pub enum AuthError {
    InvalidToken(String),
    ProviderUnavailable(String),
}

/// Call the auth provider to verify a token and get allowed subdomains.
pub async fn verify_token(
    auth: &AuthProviderConfig,
    token: &str,
    subdomain: &str,
) -> Result<Vec<String>, AuthError> {
    let resp = auth
        .client
        .post(format!("{}/api/verify", auth.url))
        .header("X-Relay-Secret", &auth.secret)
        .json(&serde_json::json!({
            "token": token,
            "subdomain": subdomain,
        }))
        .send()
        .await
        .map_err(|e| AuthError::ProviderUnavailable(e.to_string()))?;

    match resp.status().as_u16() {
        200 => {
            let body: AuthVerifyResponse = resp
                .json()
                .await
                .map_err(|e| AuthError::ProviderUnavailable(format!("bad response: {e}")))?;
            Ok(body.subdomains)
        }
        401 | 403 => {
            let msg = resp
                .json::<AuthErrorResponse>()
                .await
                .map(|r| r.error)
                .unwrap_or_else(|_| "invalid token".into());
            Err(AuthError::InvalidToken(msg))
        }
        status => Err(AuthError::ProviderUnavailable(format!(
            "unexpected status {status}"
        ))),
    }
}

/// Check if a requested subdomain matches any allowed pattern.
///
/// Supported patterns:
///   - `myapp`          exact match
///   - `myapp-*`        prefix: matches `myapp-dev`, `myapp-feat-123`
///   - `*-preview`      suffix: matches `feat-preview`, `hotfix-preview`
///   - `pr-*-mycompany` infix:  matches `pr-123-mycompany`, `pr-abc-mycompany`
///   - `*`              matches everything
pub fn subdomain_matches(patterns: &[String], subdomain: &str) -> bool {
    patterns.iter().any(|p| glob_match(p, subdomain))
}

/// Simple glob matching: `*` matches any sequence of characters (including empty).
/// Only single `*` is supported (no `**` or `?`).
fn glob_match(pattern: &str, value: &str) -> bool {
    match pattern.split_once('*') {
        None => pattern == value,
        Some((prefix, suffix)) => {
            value.starts_with(prefix)
                && value[prefix.len()..].ends_with(suffix)
                && value.len() >= prefix.len() + suffix.len()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::post;
    use axum::{Json, Router};

    /// Spin up a mock auth provider on a random port, returning its base URL.
    async fn mock_auth_provider(
        handler: axum::routing::MethodRouter,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let app = Router::new().route("/api/verify", handler);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{addr}"), handle)
    }

    fn test_auth_config(url: &str) -> AuthProviderConfig {
        AuthProviderConfig {
            url: url.to_string(),
            secret: "test-secret".to_string(),
            client: reqwest::Client::new(),
        }
    }

    // ── verify_token tests ──

    #[tokio::test]
    async fn verify_token_valid_returns_subdomains() {
        let (url, _handle) = mock_auth_provider(post(
            |headers: axum::http::HeaderMap, Json(body): Json<serde_json::Value>| async move {
                // Verify relay sends the secret header
                assert_eq!(
                    headers.get("x-relay-secret").unwrap().to_str().unwrap(),
                    "test-secret"
                );
                // Verify request body
                assert_eq!(body["token"], "valid-token");
                assert_eq!(body["subdomain"], "myapp");

                Json(serde_json::json!({
                    "subdomains": ["myapp", "myapp-*"]
                }))
            },
        ))
        .await;

        let auth = test_auth_config(&url);
        let result = verify_token(&auth, "valid-token", "myapp").await;
        let subdomains = result.unwrap();
        assert_eq!(subdomains, vec!["myapp", "myapp-*"]);
    }

    #[tokio::test]
    async fn verify_token_invalid_returns_error() {
        let (url, _handle) = mock_auth_provider(post(|| async {
            (
                axum::http::StatusCode::UNAUTHORIZED,
                Json(serde_json::json!({ "error": "token expired" })),
            )
        }))
        .await;

        let auth = test_auth_config(&url);
        let result = verify_token(&auth, "bad-token", "myapp").await;
        match result {
            Err(AuthError::InvalidToken(msg)) => assert_eq!(msg, "token expired"),
            other => panic!(
                "expected InvalidToken, got {}",
                match other {
                    Ok(_) => "Ok".to_string(),
                    Err(AuthError::ProviderUnavailable(m)) => format!("ProviderUnavailable({m})"),
                    Err(AuthError::InvalidToken(m)) => format!("InvalidToken({m})"),
                }
            ),
        }
    }

    #[tokio::test]
    async fn verify_token_forbidden_subdomain() {
        let (url, _handle) = mock_auth_provider(post(|| async {
            (
                axum::http::StatusCode::FORBIDDEN,
                Json(serde_json::json!({ "error": "subdomain_not_allowed" })),
            )
        }))
        .await;

        let auth = test_auth_config(&url);
        let result = verify_token(&auth, "valid-token", "not-mine").await;
        match result {
            Err(AuthError::InvalidToken(msg)) => assert_eq!(msg, "subdomain_not_allowed"),
            other => panic!(
                "expected InvalidToken, got {:?}",
                match other {
                    Ok(v) => format!("Ok({v:?})"),
                    Err(AuthError::ProviderUnavailable(m)) => format!("ProviderUnavailable({m})"),
                    Err(AuthError::InvalidToken(m)) => format!("InvalidToken({m})"),
                }
            ),
        }
    }

    #[tokio::test]
    async fn verify_token_provider_500_returns_unavailable() {
        let (url, _handle) = mock_auth_provider(post(|| async {
            axum::http::StatusCode::INTERNAL_SERVER_ERROR
        }))
        .await;

        let auth = test_auth_config(&url);
        let result = verify_token(&auth, "any-token", "myapp").await;
        match result {
            Err(AuthError::ProviderUnavailable(msg)) => {
                assert!(msg.contains("500"), "expected '500' in msg: {msg}");
            }
            other => panic!(
                "expected ProviderUnavailable, got {:?}",
                match other {
                    Ok(v) => format!("Ok({v:?})"),
                    Err(AuthError::InvalidToken(m)) => format!("InvalidToken({m})"),
                    Err(AuthError::ProviderUnavailable(m)) => format!("ProviderUnavailable({m})"),
                }
            ),
        }
    }

    #[tokio::test]
    async fn verify_token_provider_unreachable() {
        let auth = AuthProviderConfig {
            url: "http://127.0.0.1:1".to_string(), // nothing listening
            secret: "test-secret".to_string(),
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_millis(100))
                .build()
                .unwrap(),
        };
        let result = verify_token(&auth, "any-token", "myapp").await;
        assert!(matches!(result, Err(AuthError::ProviderUnavailable(_))));
    }

    // ── Full flow: verify_token + subdomain_matches ──

    #[tokio::test]
    async fn full_flow_valid_token_correct_subdomain() {
        let (url, _handle) = mock_auth_provider(post(|| async {
            Json(serde_json::json!({ "subdomains": ["myapp", "myapp-*"] }))
        }))
        .await;

        let auth = test_auth_config(&url);
        let allowed = verify_token(&auth, "good-token", "myapp").await.unwrap();
        assert!(subdomain_matches(&allowed, "myapp"));
        assert!(subdomain_matches(&allowed, "myapp-dev"));
        assert!(subdomain_matches(&allowed, "myapp-feat-123"));
    }

    #[tokio::test]
    async fn full_flow_valid_token_wrong_subdomain() {
        let (url, _handle) = mock_auth_provider(post(|| async {
            Json(serde_json::json!({ "subdomains": ["myapp", "myapp-*"] }))
        }))
        .await;

        let auth = test_auth_config(&url);
        let allowed = verify_token(&auth, "good-token", "other").await.unwrap();
        // Token is valid but "other" is not in the allowed list
        assert!(!subdomain_matches(&allowed, "other"));
        assert!(!subdomain_matches(&allowed, "hijack"));
    }

    // ── subdomain_matches tests ──

    #[test]
    fn subdomain_matches_exact() {
        let patterns = vec!["myapp".into(), "other".into()];
        assert!(subdomain_matches(&patterns, "myapp"));
        assert!(subdomain_matches(&patterns, "other"));
        assert!(!subdomain_matches(&patterns, "nope"));
    }

    #[test]
    fn subdomain_matches_prefix_wildcard() {
        let patterns = vec!["myapp-*".into()];
        assert!(subdomain_matches(&patterns, "myapp-dev"));
        assert!(subdomain_matches(&patterns, "myapp-feat-123"));
        assert!(subdomain_matches(&patterns, "myapp-"));
        assert!(!subdomain_matches(&patterns, "myapp"));
        assert!(!subdomain_matches(&patterns, "other"));
    }

    #[test]
    fn subdomain_matches_suffix_wildcard() {
        let patterns = vec!["*-preview".into()];
        assert!(subdomain_matches(&patterns, "feat-preview"));
        assert!(subdomain_matches(&patterns, "hotfix-preview"));
        assert!(!subdomain_matches(&patterns, "preview"));
        assert!(!subdomain_matches(&patterns, "preview-other"));
    }

    #[test]
    fn subdomain_matches_infix_wildcard() {
        let patterns = vec!["pr-*-mycompany".into()];
        assert!(subdomain_matches(&patterns, "pr-123-mycompany"));
        assert!(subdomain_matches(&patterns, "pr-abc-mycompany"));
        assert!(!subdomain_matches(&patterns, "pr-123"));
        assert!(!subdomain_matches(&patterns, "pr-mycompany"));
        assert!(!subdomain_matches(&patterns, "other-123-mycompany"));
    }

    #[test]
    fn subdomain_matches_star_matches_everything() {
        let patterns = vec!["*".into()];
        assert!(subdomain_matches(&patterns, "anything"));
        assert!(subdomain_matches(&patterns, "myapp-dev"));
        assert!(subdomain_matches(&patterns, ""));
    }

    #[test]
    fn subdomain_matches_mixed() {
        let patterns = vec!["prod".into(), "staging-*".into()];
        assert!(subdomain_matches(&patterns, "prod"));
        assert!(subdomain_matches(&patterns, "staging-v2"));
        assert!(!subdomain_matches(&patterns, "staging"));
        assert!(!subdomain_matches(&patterns, "dev"));
    }

    #[test]
    fn subdomain_matches_empty_patterns() {
        let patterns: Vec<String> = vec![];
        assert!(!subdomain_matches(&patterns, "anything"));
    }
}
