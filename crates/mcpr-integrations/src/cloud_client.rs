//! Cloud API client for mcpr.app — used by CLI onboarding and future integrations.
//!
//! JWT is held in-memory only (never persisted to disk). The only persistent
//! artifact from setup is the project-scoped `mcpr_*` token written to `mcpr.toml`.

use reqwest::Client;
use serde::{Deserialize, Serialize};

/// Default cloud API base URL.
pub const DEFAULT_CLOUD_URL: &str = "https://api.mcpr.app";

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
    pub async fn cli_verify(
        &self,
        request_id: &str,
        code: &str,
    ) -> Result<CliVerifyResponse> {
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
    pub async fn create_server(
        &self,
        project_id: &str,
        name: &str,
        slug: &str,
    ) -> Result<Server> {
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
    pub async fn list_endpoints_by_server(
        &self,
        server_id: &str,
    ) -> Result<Vec<Endpoint>> {
        self.get(&format!("/api/endpoints/by-server/{server_id}"))
            .await
    }

    /// Create a new endpoint (tunnel subdomain) for a server.
    pub async fn create_endpoint_by_server(
        &self,
        server_id: &str,
        name: &str,
    ) -> Result<Endpoint> {
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
    async fn parse_response<T: serde::de::DeserializeOwned>(
        resp: reqwest::Response,
    ) -> Result<T> {
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
