//! Interactive setup wizard for `mcpr proxy setup`.
//!
//! Authenticates via email + 6-digit code, lets the user pick a project/server,
//! creates a project-scoped token, and writes `mcpr.toml`.

use std::fmt::Write as _;

use colored::Colorize;
use inquire::{Confirm, Select, Text};
use mcpr_integrations::cloud_client::{CloudClient, DEFAULT_CLOUD_URL, Endpoint, Project, Server};

const CREATE_NEW: &str = "+ Create new";

/// Run the full setup wizard. Returns an error message on failure.
pub async fn run_setup(cloud_url: &str, output: Option<&str>) -> Result<(), String> {
    println!(
        "\n  {} Let's set up your proxy.\n",
        "Welcome to MCPR!".bold()
    );

    let mut client = CloudClient::new(cloud_url);

    // ── 1. Authenticate ────────────────────────────────────────────────
    authenticate(&mut client).await?;

    // ── 2. MCP server URL ──────────────────────────────────────────────
    let mcp_url = Text::new("MCP server URL (e.g. http://localhost:8080):")
        .prompt()
        .map_err(|e| format!("prompt error: {e}"))?;

    if mcp_url.is_empty() {
        return Err("MCP server URL is required".into());
    }

    // ── 3. Project selection ───────────────────────────────────────────
    let project = select_or_create_project(&client).await?;

    // ── 4. Server selection ────────────────────────────────────────────
    let server = select_or_create_server(&client, &project.id).await?;

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
    let config_path = output.unwrap_or("mcpr.toml");
    let config = build_config(
        &mcp_url,
        &server,
        endpoint.as_ref(),
        &token.token,
        cloud_url,
    );
    std::fs::write(config_path, &config)
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

async fn authenticate(client: &mut CloudClient) -> Result<(), String> {
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

    client.set_jwt(verify.token);

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

async fn select_or_create_server(client: &CloudClient, project_id: &str) -> Result<Server, String> {
    let servers = client
        .list_servers(project_id)
        .await
        .map_err(|e| format!("failed to list servers: {e}"))?;

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

/// Convert a name to a URL-safe slug.
fn slugify(name: &str) -> String {
    name.to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}
