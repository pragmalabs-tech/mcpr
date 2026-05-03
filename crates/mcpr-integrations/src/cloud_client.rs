//! Cloud API client for mcpr.app — used by CLI onboarding and future integrations.
//!
//! JWT is held in-memory only (never persisted to disk). The only persistent
//! artifact from setup is the project-scoped `mcpr_*` token written to `mcpr.toml`.

use reqwest::Client;
use serde::{Deserialize, Serialize};

/// Default cloud API base URL.
pub const DEFAULT_CLOUD_URL: &str = "https://api.mcpr.app";

/// Default ingest endpoint for the cloud event sink. Used when the
/// operator sets a `cloud.token` but omits `cloud.endpoint`.
pub const DEFAULT_CLOUD_INGEST_ENDPOINT: &str = "https://api.mcpr.app/api/ingest-events";

// ── Response / model types ─────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct CliLoginResponse {
    pub request_id: String,
}

#[derive(Debug, Deserialize)]
pub struct CliVerifyResponse {
    pub token: String,
    pub user: User,
}

#[derive(Debug, Clone, Deserialize)]
pub struct User {
    pub id: String,
    pub email: String,
    pub name: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Project {
    pub id: String,
    pub name: String,
    pub slug: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Server {
    pub id: String,
    pub name: String,
    pub slug: String,
    pub project_id: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Endpoint {
    pub id: String,
    pub name: String,
    pub status: String,
    pub server_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TunnelToken {
    pub id: String,
    pub token: String,
    pub name: Option<String>,
}

// ── Request bodies ─────────────────────────────────────────────────────

#[derive(Serialize)]
struct CliLoginRequest<'a> {
    email: &'a str,
}

#[derive(Serialize)]
struct CliVerifyRequest<'a> {
    request_id: &'a str,
    code: &'a str,
}

#[derive(Serialize)]
struct CreateProjectRequest<'a> {
    name: &'a str,
    slug: &'a str,
}

#[derive(Serialize)]
struct CreateServerRequest<'a> {
    name: &'a str,
    slug: &'a str,
}

#[derive(Serialize)]
struct CreateEndpointRequest<'a> {
    name: &'a str,
}

#[derive(Serialize)]
struct CreateTokenRequest<'a> {
    name: Option<&'a str>,
}

// ── Error type ─────────────────────────────────────────────────────────

#[derive(Debug)]
pub struct CloudError {
    pub status: Option<u16>,
    pub message: String,
}

impl std::fmt::Display for CloudError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(status) = self.status {
            write!(f, "cloud API error ({}): {}", status, self.message)
        } else {
            write!(f, "cloud API error: {}", self.message)
        }
    }
}

impl std::error::Error for CloudError {}

impl From<reqwest::Error> for CloudError {
    fn from(e: reqwest::Error) -> Self {
        CloudError {
            status: e.status().map(|s| s.as_u16()),
            message: e.to_string(),
        }
    }
}

type Result<T> = std::result::Result<T, CloudError>;

/// API error response body from the cloud backend.
#[derive(Deserialize)]
struct ErrorBody {
    #[serde(alias = "error")]
    message: Option<String>,
}

// ── Client ─────────────────────────────────────────────────────────────

/// Cloud API client. JWT is held in-memory only.
pub struct CloudClient {
    http: Client,
    base_url: String,
    jwt: Option<String>,
}

impl CloudClient {
    /// Create a new client pointing at the given cloud API base URL.
    pub fn new(base_url: &str) -> Self {
        Self {
            http: Client::new(),
            base_url: base_url.trim_end_matches('/').to_string(),
            jwt: None,
        }
    }

    /// Store the JWT token (in-memory only) after successful verification.
    pub fn set_jwt(&mut self, token: String) {
        self.jwt = Some(token);
    }

    /// Whether the client has been authenticated.
    pub fn is_authenticated(&self) -> bool {
        self.jwt.is_some()
    }

    // ── Auth (public, no JWT needed) ───────────────────────────────────

    /// Request a CLI login code. Sends a 6-digit verification code to the email.
    pub async fn cli_login(&self, email: &str) -> Result<CliLoginResponse> {
        let url = format!("{}/api/auth/cli/login", self.base_url);
        let resp = self
            .http
            .post(&url)
            .json(&CliLoginRequest { email })
            .send()
            .await?;
        Self::parse_response(resp).await
    }

    /// Verify the CLI login code and receive a JWT.
    pub async fn cli_verify(&self, request_id: &str, code: &str) -> Result<CliVerifyResponse> {
        let url = format!("{}/api/auth/cli/verify", self.base_url);
        let resp = self
            .http
            .post(&url)
            .json(&CliVerifyRequest { request_id, code })
            .send()
            .await?;
        Self::parse_response(resp).await
    }

    // ── Projects ───────────────────────────────────────────────────────

    /// List all projects for the authenticated user.
    pub async fn list_projects(&self) -> Result<Vec<Project>> {
        self.get("/api/projects").await
    }

    /// Create a new project.
    pub async fn create_project(&self, name: &str, slug: &str) -> Result<Project> {
        let url = format!("{}/api/projects", self.base_url);
        let resp = self
            .authed_request(reqwest::Method::POST, &url)
            .json(&CreateProjectRequest { name, slug })
            .send()
            .await?;
        Self::parse_response(resp).await
    }

    // ── Servers ────────────────────────────────────────────────────────

    /// List servers in a project.
    pub async fn list_servers(&self, project_id: &str) -> Result<Vec<Server>> {
        self.get(&format!("/api/servers/by-project/{project_id}"))
            .await
    }

    /// Create a new server in a project.
    pub async fn create_server(&self, project_id: &str, name: &str, slug: &str) -> Result<Server> {
        let url = format!("{}/api/servers/by-project/{project_id}", self.base_url);
        let resp = self
            .authed_request(reqwest::Method::POST, &url)
            .json(&CreateServerRequest { name, slug })
            .send()
            .await?;
        Self::parse_response(resp).await
    }

    // ── Endpoints ──────────────────────────────────────────────────────

    /// List endpoints for a server.
    pub async fn list_endpoints_by_server(&self, server_id: &str) -> Result<Vec<Endpoint>> {
        self.get(&format!("/api/endpoints/by-server/{server_id}"))
            .await
    }

    /// Create a new endpoint (tunnel subdomain) for a server.
    pub async fn create_endpoint_by_server(&self, server_id: &str, name: &str) -> Result<Endpoint> {
        let url = format!("{}/api/endpoints/by-server/{server_id}", self.base_url);
        let resp = self
            .authed_request(reqwest::Method::POST, &url)
            .json(&CreateEndpointRequest { name })
            .send()
            .await?;
        Self::parse_response(resp).await
    }

    // ── Tokens ─────────────────────────────────────────────────────────

    /// Create a project-scoped token (works for both tunnel auth and cloud ingest).
    pub async fn create_project_token(
        &self,
        project_id: &str,
        name: Option<&str>,
    ) -> Result<TunnelToken> {
        let url = format!("{}/api/projects/{project_id}/tokens", self.base_url);
        let resp = self
            .authed_request(reqwest::Method::POST, &url)
            .json(&CreateTokenRequest { name })
            .send()
            .await?;
        Self::parse_response(resp).await
    }

    // ── Helpers ────────────────────────────────────────────────────────

    /// Authenticated GET request.
    async fn get<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T> {
        let url = format!("{}{path}", self.base_url);
        let resp = self
            .authed_request(reqwest::Method::GET, &url)
            .send()
            .await?;
        Self::parse_response(resp).await
    }

    /// Build a request with the Bearer JWT header.
    fn authed_request(&self, method: reqwest::Method, url: &str) -> reqwest::RequestBuilder {
        let mut req = self.http.request(method, url);
        if let Some(jwt) = &self.jwt {
            req = req.bearer_auth(jwt);
        }
        req
    }

    /// Parse a response, extracting a JSON body or error message.
    async fn parse_response<T: serde::de::DeserializeOwned>(resp: reqwest::Response) -> Result<T> {
        let status = resp.status();
        if status.is_success() {
            resp.json::<T>().await.map_err(CloudError::from)
        } else {
            let code = status.as_u16();
            let body = resp.text().await.unwrap_or_default();
            let message = serde_json::from_str::<ErrorBody>(&body)
                .ok()
                .and_then(|b| b.message)
                .unwrap_or(body);
            Err(CloudError {
                status: Some(code),
                message,
            })
        }
    }
}

impl std::fmt::Display for Project {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.name)
    }
}

impl std::fmt::Display for Server {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.name)
    }
}

impl std::fmt::Display for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.name)
    }
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    // ── Helpers ───────────────────────────────────────────────────

    fn authed_client(base_url: &str) -> CloudClient {
        let mut c = CloudClient::new(base_url);
        c.set_jwt("test-jwt-token".into());
        c
    }

    // ── CloudClient::new ──────────────────────────────────────────

    #[test]
    fn new__strips_trailing_slash() {
        let c = CloudClient::new("https://api.mcpr.app/");
        assert_eq!(c.base_url, "https://api.mcpr.app");
    }

    #[test]
    fn new__starts_unauthenticated() {
        let c = CloudClient::new("https://api.mcpr.app");
        assert!(!c.is_authenticated());
        assert!(c.jwt.is_none());
    }

    // ── set_jwt / is_authenticated ───────────────────────────────

    #[test]
    fn set_jwt__makes_authenticated() {
        let mut c = CloudClient::new("http://localhost");
        c.set_jwt("tok".into());
        assert!(c.is_authenticated());
    }

    // ── cli_login ────────────────────────────────────────────────

    #[tokio::test]
    async fn cli_login__success() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/auth/cli/login"))
            .and(body_json(serde_json::json!({"email": "a@b.com"})))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"request_id": "req-123"})),
            )
            .mount(&server)
            .await;

        let client = CloudClient::new(&server.uri());
        let resp = client.cli_login("a@b.com").await.unwrap();
        assert_eq!(resp.request_id, "req-123");
    }

    #[tokio::test]
    async fn cli_login__server_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/auth/cli/login"))
            .respond_with(
                ResponseTemplate::new(400)
                    .set_body_json(serde_json::json!({"error": "invalid email"})),
            )
            .mount(&server)
            .await;

        let client = CloudClient::new(&server.uri());
        let err = client.cli_login("bad").await.unwrap_err();
        assert_eq!(err.status, Some(400));
        assert!(err.message.contains("invalid email"));
    }

    // ── cli_verify ───────────────────────────────────────────────

    #[tokio::test]
    async fn cli_verify__success() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/auth/cli/verify"))
            .and(body_json(
                serde_json::json!({"request_id": "req-1", "code": "123456"}),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "token": "jwt-xyz",
                "user": {"id": "u1", "email": "a@b.com", "name": "Alice"}
            })))
            .mount(&server)
            .await;

        let client = CloudClient::new(&server.uri());
        let resp = client.cli_verify("req-1", "123456").await.unwrap();
        assert_eq!(resp.token, "jwt-xyz");
        assert_eq!(resp.user.email, "a@b.com");
        assert_eq!(resp.user.name.as_deref(), Some("Alice"));
    }

    #[tokio::test]
    async fn cli_verify__invalid_code() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/auth/cli/verify"))
            .respond_with(
                ResponseTemplate::new(401)
                    .set_body_json(serde_json::json!({"error": "invalid code"})),
            )
            .mount(&server)
            .await;

        let client = CloudClient::new(&server.uri());
        let err = client.cli_verify("req-1", "000000").await.unwrap_err();
        assert_eq!(err.status, Some(401));
    }

    // ── list_projects ────────────────────────────────────────────

    #[tokio::test]
    async fn list_projects__sends_bearer_token() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/projects"))
            .and(header("Authorization", "Bearer test-jwt-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"id": "p1", "name": "Study Kit", "slug": "study-kit"}
            ])))
            .mount(&server)
            .await;

        let client = authed_client(&server.uri());
        let projects = client.list_projects().await.unwrap();
        assert_eq!(projects.len(), 1);
        assert_eq!(projects[0].slug, "study-kit");
    }

    #[tokio::test]
    async fn list_projects__empty_list() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/projects"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([])))
            .mount(&server)
            .await;

        let client = authed_client(&server.uri());
        let projects = client.list_projects().await.unwrap();
        assert!(projects.is_empty());
    }

    // ── create_project ───────────────────────────────────────────

    #[tokio::test]
    async fn create_project__success() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/projects"))
            .and(body_json(
                serde_json::json!({"name": "My App", "slug": "my-app"}),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "p2", "name": "My App", "slug": "my-app"
            })))
            .mount(&server)
            .await;

        let client = authed_client(&server.uri());
        let project = client.create_project("My App", "my-app").await.unwrap();
        assert_eq!(project.id, "p2");
        assert_eq!(project.slug, "my-app");
    }

    // ── list_servers ─────────────────────────────────────────────

    #[tokio::test]
    async fn list_servers__routes_to_project() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/servers/by-project/p1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"id": "s1", "name": "prod", "slug": "prod", "project_id": "p1"}
            ])))
            .mount(&server)
            .await;

        let client = authed_client(&server.uri());
        let servers = client.list_servers("p1").await.unwrap();
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].slug, "prod");
    }

    // ── create_server ────────────────────────────────────────────

    #[tokio::test]
    async fn create_server__success() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/servers/by-project/p1"))
            .and(body_json(
                serde_json::json!({"name": "staging", "slug": "staging"}),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "s2", "name": "staging", "slug": "staging", "project_id": "p1"
            })))
            .mount(&server)
            .await;

        let client = authed_client(&server.uri());
        let s = client
            .create_server("p1", "staging", "staging")
            .await
            .unwrap();
        assert_eq!(s.id, "s2");
    }

    // ── list_endpoints_by_server ─────────────────────────────────

    #[tokio::test]
    async fn list_endpoints__routes_to_server() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/endpoints/by-server/s1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([
                {"id": "e1", "name": "my-ep", "status": "active", "server_id": "s1"}
            ])))
            .mount(&server)
            .await;

        let client = authed_client(&server.uri());
        let eps = client.list_endpoints_by_server("s1").await.unwrap();
        assert_eq!(eps.len(), 1);
        assert_eq!(eps[0].name, "my-ep");
    }

    // ── create_endpoint_by_server ────────────────────────────────

    #[tokio::test]
    async fn create_endpoint__success() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/endpoints/by-server/s1"))
            .and(body_json(serde_json::json!({"name": "my-tunnel"})))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "e2", "name": "my-tunnel", "status": "active", "server_id": "s1"
            })))
            .mount(&server)
            .await;

        let client = authed_client(&server.uri());
        let ep = client
            .create_endpoint_by_server("s1", "my-tunnel")
            .await
            .unwrap();
        assert_eq!(ep.name, "my-tunnel");
    }

    // ── create_project_token ─────────────────────────────────────

    #[tokio::test]
    async fn create_project_token__success() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/projects/p1/tokens"))
            .and(body_json(serde_json::json!({"name": "cli-setup"})))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "t1", "token": "mcpr_abc123", "name": "cli-setup"
            })))
            .mount(&server)
            .await;

        let client = authed_client(&server.uri());
        let token = client
            .create_project_token("p1", Some("cli-setup"))
            .await
            .unwrap();
        assert_eq!(token.token, "mcpr_abc123");
    }

    #[tokio::test]
    async fn create_project_token__null_name() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/projects/p1/tokens"))
            .and(body_json(serde_json::json!({"name": null})))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "t2", "token": "mcpr_def456", "name": null
            })))
            .mount(&server)
            .await;

        let client = authed_client(&server.uri());
        let token = client.create_project_token("p1", None).await.unwrap();
        assert_eq!(token.token, "mcpr_def456");
        assert!(token.name.is_none());
    }

    // ── Error handling ───────────────────────────────────────────

    #[tokio::test]
    async fn parse_response__extracts_error_field() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/projects"))
            .respond_with(
                ResponseTemplate::new(403).set_body_json(serde_json::json!({"error": "forbidden"})),
            )
            .mount(&server)
            .await;

        let client = authed_client(&server.uri());
        let err = client.list_projects().await.unwrap_err();
        assert_eq!(err.status, Some(403));
        assert_eq!(err.message, "forbidden");
    }

    #[tokio::test]
    async fn parse_response__falls_back_to_raw_body() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/projects"))
            .respond_with(ResponseTemplate::new(500).set_body_string("internal failure"))
            .mount(&server)
            .await;

        let client = authed_client(&server.uri());
        let err = client.list_projects().await.unwrap_err();
        assert_eq!(err.status, Some(500));
        assert_eq!(err.message, "internal failure");
    }

    // ── Display impls ────────────────────────────────────────────

    #[test]
    fn cloud_error_display__with_status() {
        let e = CloudError {
            status: Some(404),
            message: "not found".into(),
        };
        assert_eq!(e.to_string(), "cloud API error (404): not found");
    }

    #[test]
    fn cloud_error_display__without_status() {
        let e = CloudError {
            status: None,
            message: "connection refused".into(),
        };
        assert_eq!(e.to_string(), "cloud API error: connection refused");
    }

    #[test]
    fn project_display() {
        let p = Project {
            id: "x".into(),
            name: "Study Kit".into(),
            slug: "study-kit".into(),
        };
        assert_eq!(p.to_string(), "Study Kit");
    }

    #[test]
    fn server_display() {
        let s = Server {
            id: "x".into(),
            name: "prod".into(),
            slug: "prod".into(),
            project_id: "y".into(),
        };
        assert_eq!(s.to_string(), "prod");
    }

    #[test]
    fn endpoint_display() {
        let e = Endpoint {
            id: "x".into(),
            name: "my-ep".into(),
            status: "active".into(),
            server_id: Some("s".into()),
        };
        assert_eq!(e.to_string(), "my-ep");
    }
}
