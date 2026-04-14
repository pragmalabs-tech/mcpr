//! Interactive setup wizard for `mcpr proxy setup`.
//!
//! Authenticates via email + 6-digit code, lets the user pick a project/server,
//! creates a project-scoped token, and writes `mcpr.toml`.

use std::fmt::Write as _;

use base64::Engine;
use colored::Colorize;
use inquire::{Confirm, Select, Text};
use mcpr_integrations::cloud_client::{CloudClient, DEFAULT_CLOUD_URL, Endpoint, Project, Server};

const CREATE_NEW: &str = "+ Create new";

/// Defaults extracted from an existing mcpr.toml.
#[derive(Default)]
struct ExistingConfig {
    mcp: Option<String>,
    name: Option<String>,
}

/// Try to load defaults from an existing config file.
fn load_existing_config(path: &str) -> Option<ExistingConfig> {
    let content = std::fs::read_to_string(path).ok()?;
    let table: toml::Table = content.parse().ok()?;
    Some(ExistingConfig {
        mcp: table.get("mcp").and_then(|v| v.as_str()).map(String::from),
        name: table.get("name").and_then(|v| v.as_str()).map(String::from),
    })
}

// ── JWT cache ──────────────────────────────────────────────────────────

/// Cached auth session stored at `~/.mcpr/auth.json`.
#[derive(serde::Serialize, serde::Deserialize)]
struct CachedAuth {
    jwt: String,
    email: String,
    cloud_url: String,
}

fn auth_cache_path() -> Option<std::path::PathBuf> {
    dirs::home_dir().map(|h| h.join(".mcpr").join("auth.json"))
}

fn load_cached_auth(cloud_url: &str) -> Option<CachedAuth> {
    let path = auth_cache_path()?;
    let content = std::fs::read_to_string(path).ok()?;
    let cached: CachedAuth = serde_json::from_str(&content).ok()?;

    // Must be for the same cloud URL
    if cached.cloud_url != cloud_url {
        return None;
    }

    // Check JWT expiry by decoding the payload (standard JWT: header.payload.signature)
    if is_jwt_expired(&cached.jwt) {
        return None;
    }

    Some(cached)
}

fn save_cached_auth(jwt: &str, email: &str, cloud_url: &str) {
    let Some(path) = auth_cache_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let cached = CachedAuth {
        jwt: jwt.to_string(),
        email: email.to_string(),
        cloud_url: cloud_url.to_string(),
    };
    let _ = std::fs::write(path, serde_json::to_string(&cached).unwrap_or_default());
}

/// Decode JWT payload and check `exp` claim. Returns true if expired or unparseable.
fn is_jwt_expired(jwt: &str) -> bool {
    let parts: Vec<&str> = jwt.split('.').collect();
    if parts.len() != 3 {
        return true;
    }
    let payload = match base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(parts[1]) {
        Ok(bytes) => bytes,
        Err(_) => return true,
    };
    let claims: serde_json::Value = match serde_json::from_slice(&payload) {
        Ok(v) => v,
        Err(_) => return true,
    };
    let exp = match claims.get("exp").and_then(|v| v.as_i64()) {
        Some(e) => e,
        None => return true,
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    // Expire 5 minutes early to avoid edge cases
    now >= exp - 300
}

// ── Main flow ──────────────────────────────────────────────────────────

/// Run the full setup wizard. Returns an error message on failure.
pub async fn run_setup(cloud_url: &str, output: Option<&str>) -> Result<(), String> {
    println!(
        "\n  {} Let's set up your proxy.\n",
        "Welcome to MCPR!".bold()
    );

    // ── Check for existing config ──────────────────────────────────────
    let defaults = if output.is_none() && std::path::Path::new("mcpr.toml").exists() {
        let reuse = Confirm::new("Found existing mcpr.toml. Use its settings as defaults?")
            .with_default(true)
            .prompt()
            .map_err(|e| format!("prompt error: {e}"))?;
        if reuse {
            load_existing_config("mcpr.toml").unwrap_or_default()
        } else {
            ExistingConfig::default()
        }
    } else {
        ExistingConfig::default()
    };

    let mut client = CloudClient::new(cloud_url);

    // ── 1. Authenticate (try cached JWT first) ─────────────────────────
    if let Some(cached) = load_cached_auth(cloud_url) {
        client.set_jwt(cached.jwt);
        println!(
            "  {} Logged in as {} (cached)\n",
            "✓".green(),
            cached.email.bold()
        );
    } else {
        authenticate(&mut client, cloud_url).await?;
    }

    // ── 2. MCP server URL ──────────────────────────────────────────────
    let mut prompt = Text::new("MCP server URL (e.g. http://localhost:8080):");
    if let Some(ref mcp) = defaults.mcp {
        prompt = prompt.with_default(mcp);
    }
    let mcp_url = prompt.prompt().map_err(|e| format!("prompt error: {e}"))?;

    if mcp_url.is_empty() {
        return Err("MCP server URL is required".into());
    }

    // ── 3. Project selection ───────────────────────────────────────────
    let project = select_or_create_project(&client).await?;

    // ── 4. Server selection ────────────────────────────────────────────
    let server = select_or_create_server(&client, &project.id, &defaults).await?;

    // ── 5. Tunnel ──────────────────────────────────────────────────────
    let enable_tunnel = Confirm::new("Enable tunnel?")
        .with_default(true)
        .prompt()
        .map_err(|e| format!("prompt error: {e}"))?;

    let endpoint = if enable_tunnel {
        Some(select_or_create_endpoint(&client, &server).await?)
    } else {
        None
    };

    // ── 6. Create project token ────────────────────────────────────────
    println!("\n  Setting up...");
    let token_name = format!("cli-setup-{}", server.slug);
    let token = client
        .create_project_token(&project.id, Some(&token_name))
        .await
        .map_err(|e| format!("failed to create token: {e}"))?;
    println!("    {} Created project token", "✓".green());

    // ── 7. Write config ────────────────────────────────────────────────
    let config_path = match output {
        Some(explicit) => explicit.to_string(),
        None => next_available_config_path(),
    };
    let config = build_config(
        &mcp_url,
        &server,
        endpoint.as_ref(),
        &token.token,
        cloud_url,
    );
    std::fs::write(&config_path, &config)
        .map_err(|e| format!("failed to write {config_path}: {e}"))?;
    println!("    {} Saved {}", "✓".green(), config_path);

    // ── Done ───────────────────────────────────────────────────────────
    println!("\n  {}", "Run your proxy:".bold());
    println!("    mcpr start && mcpr proxy run -c {config_path}");

    if let Some(ep) = &endpoint {
        println!(
            "\n  {} https://{}.tunnel.mcpr.app",
            "Tunnel URL:".bold(),
            ep.name
        );
    }

    println!(
        "  {} https://mcpr.app/projects/{}/servers/{}",
        "Dashboard: ".bold(),
        project.slug,
        server.slug
    );
    println!();

    Ok(())
}

// ── Authentication ─────────────────────────────────────────────────────

async fn authenticate(client: &mut CloudClient, cloud_url: &str) -> Result<(), String> {
    let email = Text::new("Your email:")
        .prompt()
        .map_err(|e| format!("prompt error: {e}"))?;

    if !email.contains('@') || email.len() < 5 {
        return Err("invalid email address".into());
    }

    print!("  Sending verification code... ");
    let login = client
        .cli_login(&email)
        .await
        .map_err(|e| format!("login failed: {e}"))?;
    println!("{}", "✓".green());
    println!(
        "  {}",
        "Check your email for a 6-digit verification code.".dimmed()
    );

    let code = Text::new("Enter the 6-digit code:")
        .prompt()
        .map_err(|e| format!("prompt error: {e}"))?;

    let verify = client
        .cli_verify(&login.request_id, &code)
        .await
        .map_err(|e| format!("verification failed: {e}"))?;

    client.set_jwt(verify.token.clone());
    save_cached_auth(&verify.token, &email, cloud_url);

    let name = verify.user.name.as_deref().unwrap_or(&verify.user.email);
    println!("  {} Verified! Welcome, {}.\n", "✓".green(), name.bold());

    Ok(())
}

// ── Project ────────────────────────────────────────────────────────────

async fn select_or_create_project(client: &CloudClient) -> Result<Project, String> {
    let projects = client
        .list_projects()
        .await
        .map_err(|e| format!("failed to list projects: {e}"))?;

    if projects.is_empty() {
        println!("  No projects found. Let's create one.");
        return create_project(client).await;
    }

    let mut options: Vec<String> = projects.iter().map(|p| p.name.clone()).collect();
    options.push(CREATE_NEW.to_string());

    let choice = Select::new("Choose project:", options)
        .prompt()
        .map_err(|e| format!("prompt error: {e}"))?;

    if choice == CREATE_NEW {
        create_project(client).await
    } else {
        projects
            .into_iter()
            .find(|p| p.name == choice)
            .ok_or_else(|| "project not found".into())
    }
}

async fn create_project(client: &CloudClient) -> Result<Project, String> {
    let name = Text::new("Project name:")
        .prompt()
        .map_err(|e| format!("prompt error: {e}"))?;
    let slug = slugify(&name);
    let slug = Text::new("Project slug:")
        .with_default(&slug)
        .prompt()
        .map_err(|e| format!("prompt error: {e}"))?;

    client
        .create_project(&name, &slug)
        .await
        .map_err(|e| format!("failed to create project: {e}"))
}

// ── Server ─────────────────────────────────────────────────────────────

async fn select_or_create_server(
    client: &CloudClient,
    project_id: &str,
    defaults: &ExistingConfig,
) -> Result<Server, String> {
    let servers = client
        .list_servers(project_id)
        .await
        .map_err(|e| format!("failed to list servers: {e}"))?;

    // If existing config has a name that matches a server, pre-select it
    if let Some(ref default_name) = defaults.name
        && let Some(matching) = servers.iter().find(|s| s.slug == *default_name)
    {
        let reuse = Confirm::new(&format!("Use existing server \"{}\"?", matching.name))
            .with_default(true)
            .prompt()
            .map_err(|e| format!("prompt error: {e}"))?;
        if reuse {
            return Ok(matching.clone());
        }
    }

    if servers.is_empty() {
        println!("  No servers found. Let's create one.");
        return create_server(client, project_id).await;
    }

    let mut options: Vec<String> = servers.iter().map(|s| s.name.clone()).collect();
    options.push(CREATE_NEW.to_string());

    let choice = Select::new("Choose server:", options)
        .prompt()
        .map_err(|e| format!("prompt error: {e}"))?;

    if choice == CREATE_NEW {
        create_server(client, project_id).await
    } else {
        servers
            .into_iter()
            .find(|s| s.name == choice)
            .ok_or_else(|| "server not found".into())
    }
}

async fn create_server(client: &CloudClient, project_id: &str) -> Result<Server, String> {
    let name = Text::new("Server name:")
        .prompt()
        .map_err(|e| format!("prompt error: {e}"))?;
    let slug = slugify(&name);
    let slug = Text::new("Server slug:")
        .with_default(&slug)
        .prompt()
        .map_err(|e| format!("prompt error: {e}"))?;

    client
        .create_server(project_id, &name, &slug)
        .await
        .map_err(|e| format!("failed to create server: {e}"))
}

// ── Endpoint ───────────────────────────────────────────────────────────

async fn select_or_create_endpoint(
    client: &CloudClient,
    server: &Server,
) -> Result<Endpoint, String> {
    let endpoints = client
        .list_endpoints_by_server(&server.id)
        .await
        .map_err(|e| format!("failed to list endpoints: {e}"))?;

    if endpoints.is_empty() {
        return create_endpoint(client, server).await;
    }

    let mut options: Vec<String> = endpoints
        .iter()
        .map(|e| format!("{} ({}.tunnel.mcpr.app)", e.name, e.name))
        .collect();
    options.push(CREATE_NEW.to_string());

    let choice = Select::new("Choose endpoint:", options)
        .prompt()
        .map_err(|e| format!("prompt error: {e}"))?;

    if choice == CREATE_NEW {
        create_endpoint(client, server).await
    } else {
        // Extract the endpoint name from the display string "name (name.tunnel.mcpr.app)"
        let ep_name = choice.split(' ').next().unwrap_or(&choice);
        endpoints
            .into_iter()
            .find(|e| e.name == ep_name)
            .ok_or_else(|| "endpoint not found".into())
    }
}

async fn create_endpoint(client: &CloudClient, server: &Server) -> Result<Endpoint, String> {
    let default_name = server.slug.clone();
    let name = Text::new("Endpoint subdomain:")
        .with_default(&default_name)
        .prompt()
        .map_err(|e| format!("prompt error: {e}"))?;

    let ep = client
        .create_endpoint_by_server(&server.id, &name)
        .await
        .map_err(|e| format!("failed to create endpoint: {e}"))?;

    println!(
        "    {} Created endpoint: {}.tunnel.mcpr.app",
        "✓".green(),
        ep.name
    );
    Ok(ep)
}

// ── Config generation ──────────────────────────────────────────────────

fn build_config(
    mcp_url: &str,
    server: &Server,
    endpoint: Option<&Endpoint>,
    token: &str,
    cloud_url: &str,
) -> String {
    let mut cfg = String::new();
    writeln!(cfg, "name = \"{}\"", server.slug).unwrap();
    writeln!(cfg, "mcp = \"{}\"", mcp_url).unwrap();

    if let Some(ep) = endpoint {
        writeln!(cfg).unwrap();
        writeln!(cfg, "[tunnel]").unwrap();
        writeln!(cfg, "enabled = true").unwrap();
        writeln!(cfg, "token = \"{}\"", token).unwrap();
        writeln!(cfg, "subdomain = \"{}\"", ep.name).unwrap();
    }

    writeln!(cfg).unwrap();
    writeln!(cfg, "[cloud]").unwrap();
    writeln!(cfg, "token = \"{}\"", token).unwrap();
    writeln!(cfg, "server = \"{}\"", server.slug).unwrap();
    if cloud_url != DEFAULT_CLOUD_URL {
        writeln!(cfg, "endpoint = \"{}\"", cloud_url).unwrap();
    }

    cfg
}

// ── Helpers ────────────────────────────────────────────────────────────

/// Find the next available config filename: mcpr.toml, mcpr_2.toml, mcpr_3.toml, ...
fn next_available_config_path() -> String {
    next_available_config_in(".")
}

fn next_available_config_in(dir: &str) -> String {
    let base = std::path::Path::new(dir).join("mcpr.toml");
    if !base.exists() {
        return "mcpr.toml".to_string();
    }
    for i in 2.. {
        let name = format!("mcpr_{i}.toml");
        if !std::path::Path::new(dir).join(&name).exists() {
            return name;
        }
    }
    unreachable!()
}

/// Convert a name to a URL-safe slug.
fn slugify(name: &str) -> String {
    name.to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;

    // ── Helper: build a JWT with a given exp ─────────────────────────

    fn make_jwt(exp: i64) -> String {
        let header = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(r#"{"alg":"HS256","typ":"JWT"}"#);
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(format!(r#"{{"sub":"u1","email":"a@b.com","exp":{exp}}}"#));
        format!("{header}.{payload}.fake-signature")
    }

    fn now_secs() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64
    }

    // ── is_jwt_expired ──────────────────────────────────────────────

    #[test]
    fn is_jwt_expired__future_token() {
        let jwt = make_jwt(now_secs() + 3600);
        assert!(!is_jwt_expired(&jwt));
    }

    #[test]
    fn is_jwt_expired__past_token() {
        let jwt = make_jwt(now_secs() - 100);
        assert!(is_jwt_expired(&jwt));
    }

    #[test]
    fn is_jwt_expired__within_5min_buffer() {
        // Expires in 4 minutes — should be treated as expired (5-min buffer)
        let jwt = make_jwt(now_secs() + 240);
        assert!(is_jwt_expired(&jwt));
    }

    #[test]
    fn is_jwt_expired__just_outside_buffer() {
        // Expires in 6 minutes — should NOT be expired
        let jwt = make_jwt(now_secs() + 360);
        assert!(!is_jwt_expired(&jwt));
    }

    #[test]
    fn is_jwt_expired__garbage_input() {
        assert!(is_jwt_expired("not-a-jwt"));
        assert!(is_jwt_expired(""));
        assert!(is_jwt_expired("a.b"));
        assert!(is_jwt_expired("a.!!!.c"));
    }

    #[test]
    fn is_jwt_expired__missing_exp_claim() {
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(r#"{"sub":"u1"}"#);
        let jwt = format!("header.{payload}.sig");
        assert!(is_jwt_expired(&jwt));
    }

    // ── slugify ─────────────────────────────────────────────────────

    #[test]
    fn slugify__simple_name() {
        assert_eq!(slugify("My Project"), "my-project");
    }

    #[test]
    fn slugify__special_chars() {
        assert_eq!(slugify("Hello World! (v2)"), "hello-world---v2");
    }

    #[test]
    fn slugify__already_slug() {
        assert_eq!(slugify("my-project"), "my-project");
    }

    #[test]
    fn slugify__leading_trailing_hyphens() {
        assert_eq!(slugify("--test--"), "test");
    }

    #[test]
    fn slugify__mixed_case() {
        assert_eq!(slugify("StudyKit"), "studykit");
    }

    // ── build_config ────────────────────────────────────────────────

    #[test]
    fn build_config__with_tunnel() {
        let server = Server {
            id: "s1".into(),
            name: "prod".into(),
            slug: "prod".into(),
            project_id: "p1".into(),
        };
        let endpoint = Endpoint {
            id: "e1".into(),
            name: "my-app".into(),
            status: "active".into(),
            server_id: Some("s1".into()),
        };
        let config = build_config(
            "http://localhost:8080",
            &server,
            Some(&endpoint),
            "mcpr_token123",
            DEFAULT_CLOUD_URL,
        );
        assert!(config.contains("name = \"prod\""));
        assert!(config.contains("mcp = \"http://localhost:8080\""));
        assert!(config.contains("[tunnel]"));
        assert!(config.contains("enabled = true"));
        assert!(config.contains("token = \"mcpr_token123\""));
        assert!(config.contains("subdomain = \"my-app\""));
        assert!(config.contains("[cloud]"));
        assert!(config.contains("server = \"prod\""));
        // Default cloud URL should NOT produce an endpoint line
        assert!(!config.contains("endpoint ="));
    }

    #[test]
    fn build_config__without_tunnel() {
        let server = Server {
            id: "s1".into(),
            name: "dev".into(),
            slug: "dev".into(),
            project_id: "p1".into(),
        };
        let config = build_config(
            "http://localhost:3000",
            &server,
            None,
            "mcpr_abc",
            DEFAULT_CLOUD_URL,
        );
        assert!(config.contains("name = \"dev\""));
        assert!(config.contains("mcp = \"http://localhost:3000\""));
        assert!(!config.contains("[tunnel]"));
        assert!(config.contains("[cloud]"));
        assert!(config.contains("token = \"mcpr_abc\""));
    }

    #[test]
    fn build_config__custom_cloud_url() {
        let server = Server {
            id: "s1".into(),
            name: "dev".into(),
            slug: "dev".into(),
            project_id: "p1".into(),
        };
        let config = build_config(
            "http://localhost:3000",
            &server,
            None,
            "mcpr_abc",
            "http://localhost:8000",
        );
        assert!(config.contains("endpoint = \"http://localhost:8000\""));
    }

    // ── load_existing_config ────────────────────────────────────────

    #[test]
    fn load_existing_config__valid_toml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcpr.toml");
        std::fs::write(
            &path,
            "name = \"my-server\"\nmcp = \"http://localhost:8080\"\n",
        )
        .unwrap();

        let cfg = load_existing_config(path.to_str().unwrap()).unwrap();
        assert_eq!(cfg.mcp.as_deref(), Some("http://localhost:8080"));
        assert_eq!(cfg.name.as_deref(), Some("my-server"));
    }

    #[test]
    fn load_existing_config__missing_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcpr.toml");
        std::fs::write(&path, "port = 3000\n").unwrap();

        let cfg = load_existing_config(path.to_str().unwrap()).unwrap();
        assert!(cfg.mcp.is_none());
        assert!(cfg.name.is_none());
    }

    #[test]
    fn load_existing_config__missing_file() {
        assert!(load_existing_config("/nonexistent/mcpr.toml").is_none());
    }

    #[test]
    fn load_existing_config__invalid_toml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcpr.toml");
        std::fs::write(&path, "this is not valid toml {{{{").unwrap();

        assert!(load_existing_config(path.to_str().unwrap()).is_none());
    }

    // ── next_available_config_in ───────────────────────────────────

    #[test]
    fn next_available_config__no_existing() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            next_available_config_in(dir.path().to_str().unwrap()),
            "mcpr.toml"
        );
    }

    #[test]
    fn next_available_config__one_existing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("mcpr.toml"), "").unwrap();
        assert_eq!(
            next_available_config_in(dir.path().to_str().unwrap()),
            "mcpr_2.toml"
        );
    }

    #[test]
    fn next_available_config__gap_in_sequence() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("mcpr.toml"), "").unwrap();
        std::fs::write(dir.path().join("mcpr_2.toml"), "").unwrap();
        // mcpr_3.toml missing
        std::fs::write(dir.path().join("mcpr_4.toml"), "").unwrap();
        assert_eq!(
            next_available_config_in(dir.path().to_str().unwrap()),
            "mcpr_3.toml"
        );
    }

    // ── CachedAuth roundtrip ────────────────────────────────────────

    #[test]
    fn cached_auth__serialization_roundtrip() {
        let cached = CachedAuth {
            jwt: "eyJ.payload.sig".into(),
            email: "a@b.com".into(),
            cloud_url: "https://api.mcpr.app".into(),
        };
        let json = serde_json::to_string(&cached).unwrap();
        let parsed: CachedAuth = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.jwt, "eyJ.payload.sig");
        assert_eq!(parsed.email, "a@b.com");
        assert_eq!(parsed.cloud_url, "https://api.mcpr.app");
    }
}
