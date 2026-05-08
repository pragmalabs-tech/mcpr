//! Interactive setup flow for `mcpr proxy setup`.
//!
//! Authenticates via email + 6-digit code, lets the user pick a project/server,
//! creates a project-scoped token, and writes `mcpr.toml`.

use std::fmt::Write as _;

use base64::Engine;
use colored::Colorize;
use inquire::{Confirm, Select, Text};
use mcpr_integrations::cloud_client::{CloudClient, DEFAULT_CLOUD_URL, Project, Server};

const CREATE_NEW: &str = "+ Create new";

/// Defaults extracted from an existing mcpr.toml.
#[derive(Default)]
struct ExistingConfig {
    mcp: Option<String>,
    name: Option<String>,
    cloud_token: Option<String>,
    cloud_server: Option<String>,
}

/// Try to load defaults from an existing config file.
fn load_existing_config(path: &str) -> Option<ExistingConfig> {
    let content = std::fs::read_to_string(path).ok()?;
    let table: toml::Table = content.parse().ok()?;
    let cloud = table.get("cloud").and_then(|v| v.as_table());
    Some(ExistingConfig {
        mcp: table.get("mcp").and_then(|v| v.as_str()).map(String::from),
        name: table.get("name").and_then(|v| v.as_str()).map(String::from),
        cloud_token: cloud
            .and_then(|c| c.get("token"))
            .and_then(|v| v.as_str())
            .map(String::from),
        cloud_server: cloud
            .and_then(|c| c.get("server"))
            .and_then(|v| v.as_str())
            .map(String::from),
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

/// Run the full setup flow. Returns an error message on failure.
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
    let mut authed = false;

    // ── 1. MCP server URL ──────────────────────────────────────────────
    let mut prompt = Text::new("MCP server URL (e.g. http://localhost:8080):");
    if let Some(ref mcp) = defaults.mcp {
        prompt = prompt.with_default(mcp);
    }
    let mcp_url = prompt.prompt().map_err(|e| format!("prompt error: {e}"))?;

    if mcp_url.is_empty() {
        return Err("MCP server URL is required".into());
    }

    // ── 2. Listen port ─────────────────────────────────────────────────
    let port: Option<u16> = loop {
        let port_str = Text::new("Listen port (leave empty for auto):")
            .with_default("")
            .prompt()
            .map_err(|e| format!("prompt error: {e}"))?;
        if port_str.is_empty() {
            break None;
        }
        let p: u16 = match port_str.parse() {
            Ok(v) => v,
            Err(_) => {
                println!("  {} invalid port number: {port_str}", "✗".red());
                continue;
            }
        };
        if is_port_available(p) {
            break Some(p);
        }
        println!("  {} port {p} is already in use", "✗".red());
    };

    // ── 3. Cloud dashboard? (login on yes) ─────────────────────────────
    println!();
    println!(
        "  {}",
        "Debug slow tool calls, track performance, and inspect errors in a web UI.".dimmed()
    );
    println!(
        "  {}",
        "Docs: https://mcpr.app/cloud/server-overview/".dimmed()
    );
    let enable_cloud = Confirm::new("Send request metrics to the mcpr.app dashboard?")
        .with_default(false)
        .prompt()
        .map_err(|e| format!("prompt error: {e}"))?;

    if enable_cloud {
        ensure_authenticated(&mut client, cloud_url, &mut authed).await?;
    }

    // ── 4. Local-only shortcut ─────────────────────────────────────────
    if !enable_cloud {
        let name = default_local_name(&defaults, &mcp_url);
        let config_path = match output {
            Some(explicit) => explicit.to_string(),
            None => next_available_config_path(),
        };
        let config = build_config(&name, &mcp_url, port, None);
        std::fs::write(&config_path, &config)
            .map_err(|e| format!("failed to write {config_path}: {e}"))?;
        println!("\n    {} Saved {}", "✓".green(), config_path);
        println!("\n  {}", "Run your proxy:".bold());
        println!("    mcpr proxy run {config_path}");
        println!();
        return Ok(());
    }

    // ── 5. Project + server (needed for cloud) ─────────────────────────
    let project = select_or_create_project(&client).await?;
    let server = select_or_create_server(&client, &project.id, &defaults).await?;

    // ── 6. Create or reuse project token ───────────────────────────────
    println!("\n  Setting up...");

    let token_value = if let Some(ref existing_token) = defaults.cloud_token
        && defaults.cloud_server.as_deref() == Some(&server.slug)
    {
        let reuse = Confirm::new("Reuse existing project token?")
            .with_default(true)
            .prompt()
            .map_err(|e| format!("prompt error: {e}"))?;
        if reuse {
            println!("    {} Reusing existing project token", "✓".green());
            existing_token.clone()
        } else {
            let token_name = format!("cli-setup-{}", server.slug);
            let token = client
                .create_project_token(&project.id, Some(&token_name))
                .await
                .map_err(|e| format!("failed to create token: {e}"))?;
            println!("    {} Created project token", "✓".green());
            token.token
        }
    } else {
        let token_name = format!("cli-setup-{}", server.slug);
        let token = client
            .create_project_token(&project.id, Some(&token_name))
            .await
            .map_err(|e| format!("failed to create token: {e}"))?;
        println!("    {} Created project token", "✓".green());
        token.token
    };

    // ── 7. Write config ────────────────────────────────────────────────
    let config_path = match output {
        Some(explicit) => explicit.to_string(),
        None => next_available_config_path(),
    };
    let cloud_cfg = Some(CloudCfg {
        server_slug: &server.slug,
        token: &token_value,
        cloud_url,
    });
    let config = build_config(&server.slug, &mcp_url, port, cloud_cfg);
    std::fs::write(&config_path, &config)
        .map_err(|e| format!("failed to write {config_path}: {e}"))?;
    println!("    {} Saved {}", "✓".green(), config_path);

    // ── Done ───────────────────────────────────────────────────────────
    println!("\n  {}", "Run your proxy:".bold());
    println!("    mcpr proxy run {config_path}");

    println!(
        "  {} https://cloud.mcpr.app/projects/{}/servers/{}",
        "Dashboard: ".bold(),
        project.slug,
        server.slug
    );
    println!();

    Ok(())
}

/// Lazily authenticate the cloud client. No-op if already authed this run.
/// Tries cached JWT first; otherwise prompts email + 6-digit code.
async fn ensure_authenticated(
    client: &mut CloudClient,
    cloud_url: &str,
    authed: &mut bool,
) -> Result<(), String> {
    if *authed {
        return Ok(());
    }
    if let Some(cached) = load_cached_auth(cloud_url) {
        client.set_jwt(cached.jwt);
        println!(
            "  {} Logged in as {} (cached)",
            "✓".green(),
            cached.email.bold()
        );
    } else {
        authenticate(client, cloud_url).await?;
    }
    *authed = true;
    Ok(())
}

/// Derive a proxy name when no cloud server is selected. Falls back to the
/// existing config's name, then to a slug of the MCP URL's host:port.
fn default_local_name(defaults: &ExistingConfig, mcp_url: &str) -> String {
    if let Some(ref n) = defaults.name {
        return n.clone();
    }
    let host = mcp_url
        .trim_start_matches("http://")
        .trim_start_matches("https://")
        .trim_end_matches('/')
        .split('/')
        .next()
        .unwrap_or("proxy");
    let slug = slugify(host);
    if slug.is_empty() {
        "proxy".to_string()
    } else {
        slug
    }
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

// ── Config generation ──────────────────────────────────────────────────

struct CloudCfg<'a> {
    server_slug: &'a str,
    token: &'a str,
    cloud_url: &'a str,
}

fn build_config(name: &str, mcp_url: &str, port: Option<u16>, cloud: Option<CloudCfg>) -> String {
    let mut cfg = String::new();
    writeln!(cfg, "name = \"{}\"", name).unwrap();
    writeln!(cfg, "mcp = \"{}\"", mcp_url).unwrap();
    if let Some(p) = port {
        writeln!(cfg, "port = {}", p).unwrap();
    }

    if let Some(c) = cloud {
        writeln!(cfg).unwrap();
        writeln!(cfg, "[cloud]").unwrap();
        writeln!(cfg, "token = \"{}\"", c.token).unwrap();
        writeln!(cfg, "server = \"{}\"", c.server_slug).unwrap();
        if c.cloud_url != DEFAULT_CLOUD_URL {
            writeln!(cfg, "endpoint = \"{}\"", c.cloud_url).unwrap();
        }
    }

    cfg
}

// ── Helpers ────────────────────────────────────────────────────────────

/// Check whether a TCP port is available for binding.
fn is_port_available(port: u16) -> bool {
    std::net::TcpListener::bind(("0.0.0.0", port)).is_ok()
}

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
    fn build_config__cloud_only() {
        let config = build_config(
            "dev",
            "http://localhost:3000",
            None,
            Some(CloudCfg {
                server_slug: "dev",
                token: "mcpr_abc",
                cloud_url: DEFAULT_CLOUD_URL,
            }),
        );
        assert!(config.contains("name = \"dev\""));
        assert!(config.contains("mcp = \"http://localhost:3000\""));
        assert!(config.contains("[cloud]"));
        assert!(config.contains("token = \"mcpr_abc\""));
    }

    #[test]
    fn build_config__local_only() {
        let config = build_config("local", "http://localhost:3000", Some(4000), None);
        assert!(config.contains("name = \"local\""));
        assert!(config.contains("mcp = \"http://localhost:3000\""));
        assert!(config.contains("port = 4000"));
        assert!(!config.contains("[cloud]"));
    }

    #[test]
    fn build_config__custom_cloud_url() {
        let config = build_config(
            "dev",
            "http://localhost:3000",
            Some(8080),
            Some(CloudCfg {
                server_slug: "dev",
                token: "mcpr_abc",
                cloud_url: "http://localhost:8000",
            }),
        );
        assert!(config.contains("endpoint = \"http://localhost:8000\""));
        assert!(config.contains("port = 8080"));
    }

    #[test]
    fn build_config__with_explicit_port() {
        let config = build_config(
            "dev",
            "http://localhost:3000",
            Some(4000),
            Some(CloudCfg {
                server_slug: "dev",
                token: "mcpr_abc",
                cloud_url: DEFAULT_CLOUD_URL,
            }),
        );
        assert!(config.contains("port = 4000"));
    }

    #[test]
    fn build_config__no_port_omits_line() {
        let config = build_config(
            "dev",
            "http://localhost:3000",
            None,
            Some(CloudCfg {
                server_slug: "dev",
                token: "mcpr_abc",
                cloud_url: DEFAULT_CLOUD_URL,
            }),
        );
        assert!(!config.contains("port ="));
    }

    // ── default_local_name ──────────────────────────────────────────

    #[test]
    fn default_local_name__uses_existing_name() {
        let defaults = ExistingConfig {
            name: Some("my-existing".into()),
            ..Default::default()
        };
        assert_eq!(
            default_local_name(&defaults, "http://localhost:8080"),
            "my-existing"
        );
    }

    #[test]
    fn default_local_name__derives_from_url() {
        let defaults = ExistingConfig::default();
        assert_eq!(
            default_local_name(&defaults, "http://localhost:8080"),
            "localhost-8080"
        );
    }

    #[test]
    fn default_local_name__strips_path() {
        let defaults = ExistingConfig::default();
        assert_eq!(
            default_local_name(&defaults, "http://api.example.com/v1"),
            "api-example-com"
        );
    }

    #[test]
    fn default_local_name__fallback_on_empty() {
        let defaults = ExistingConfig::default();
        assert_eq!(default_local_name(&defaults, ""), "proxy");
    }

    // ── is_port_available ──────────────────────────────────────────

    #[test]
    fn is_port_available__free_port() {
        // Port 0 always succeeds (OS picks an available port).
        assert!(is_port_available(0));
    }

    #[test]
    fn is_port_available__bound_port_is_unavailable() {
        let listener = std::net::TcpListener::bind("0.0.0.0:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(!is_port_available(port));
    }

    // ── load_existing_config ────────────────────────────────────────

    #[test]
    fn load_existing_config__valid_toml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcpr.toml");
        std::fs::write(
            &path,
            "name = \"my-server\"\nmcp = \"http://localhost:8080\"\n\n[cloud]\ntoken = \"mcpr_abc\"\nserver = \"my-server\"\n",
        )
        .unwrap();

        let cfg = load_existing_config(path.to_str().unwrap()).unwrap();
        assert_eq!(cfg.mcp.as_deref(), Some("http://localhost:8080"));
        assert_eq!(cfg.name.as_deref(), Some("my-server"));
        assert_eq!(cfg.cloud_token.as_deref(), Some("mcpr_abc"));
        assert_eq!(cfg.cloud_server.as_deref(), Some("my-server"));
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
