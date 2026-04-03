use clap::Parser;

use mcpr_tunnel::RelayConfig;
pub use mcpr_widgets::{CspMode, parse_csp_mode};

const CONFIG_FILE: &str = "mcpr.toml";

// ── Run mode ────────────────────────────────────────────────────────────

/// Top-level mode: either run as a relay server or as the gateway proxy.
pub enum Mode {
    Relay(RelayConfig),
    Gateway(GatewayConfig),
}

// ── CLI args ────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(
    name = "mcpr",
    version,
    about = "Open-source proxy for MCP Apps — fixes CSP, handles auth, observes every tool call."
)]
struct Cli {
    /// Upstream MCP server URL
    #[arg(long)]
    mcp: Option<String>,

    /// Widget source: URL (proxy mode) or path (static mode)
    #[arg(long)]
    widgets: Option<String>,

    /// Local proxy port
    #[arg(long)]
    port: Option<u16>,

    /// Extra CSP domains
    #[arg(long = "csp")]
    csp_domains: Vec<String>,

    /// CSP mode: "extend" (add to upstream CSP) or "override" (replace upstream CSP)
    #[arg(long = "csp-mode")]
    csp_mode: Option<String>,

    /// Run as relay server instead of client proxy
    #[arg(long)]
    relay: bool,

    /// Relay server base domain (for relay mode)
    #[arg(long)]
    relay_domain: Option<String>,

    /// Auth provider URL for token validation (relay mode)
    #[arg(long, env = "MCPR_AUTH_PROVIDER")]
    auth_provider: Option<String>,

    /// Shared secret between relay and auth provider
    #[arg(long, env = "MCPR_AUTH_PROVIDER_SECRET")]
    auth_provider_secret: Option<String>,

    /// Relay server URL (for gateway tunnel mode)
    #[arg(long, env = "MCPR_RELAY_URL")]
    relay_url: Option<String>,

    /// Don't start any tunnel (local-only mode)
    #[arg(long)]
    no_tunnel: bool,

    /// Emit structured JSON events to stdout
    #[arg(long)]
    events: bool,
}

// ── TOML config file ────────────────────────────────────────────────────

/// `[csp]` table in config file
#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct FileCspConfig {
    mode: Option<String>,
    domains: Vec<String>,
}

/// Entry in `[[relay.tokens]]` array
#[derive(serde::Deserialize)]
struct FileTokenEntry {
    token: String,
    subdomains: Vec<String>,
}

/// `[relay]` table in config file
#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct FileRelayConfig {
    domain: Option<String>,
    auth_provider: Option<String>,
    auth_provider_secret: Option<String>,
    tokens: Vec<FileTokenEntry>,
}

/// `[tunnel]` table in config file
#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct FileTunnelConfig {
    relay_url: Option<String>,
    token: Option<String>,
    subdomain: Option<String>,
    anonymous: bool,
}

/// `[events]` table in config file
#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct FileEventsConfig {
    enabled: bool,
}

/// `[logging]` table in config file
#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct FileLoggingConfig {
    /// Enable JSONL file logging.
    file: bool,
    /// Directory for log files (default: "./logs").
    dir: Option<String>,
    /// Rotation strategy: "daily" or "size:50MB" (default: "daily").
    rotation: Option<String>,
}

/// Config file format (mcpr.toml)
#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct FileConfig {
    // -- Shared --
    port: Option<u16>,
    mode: Option<String>, // "relay" | "gateway" (default)

    // -- Gateway --
    mcp: Option<String>,
    widgets: Option<String>,
    no_tunnel: bool,
    csp: FileCspConfig,

    // -- Relay --
    relay: FileRelayConfig,

    // -- Tunnel client --
    tunnel: FileTunnelConfig,

    // -- Events --
    events: FileEventsConfig,

    // -- Logging --
    logging: FileLoggingConfig,

    max_request_body_size: Option<usize>,
    max_response_body_size: Option<usize>,
    max_concurrent_upstream: Option<usize>,
    connect_timeout: Option<u64>,
    request_timeout: Option<u64>,

    // -- Legacy flat fields (backward compat) --
    relay_domain: Option<String>,
    relay_url: Option<String>,
    tunnel_token: Option<String>,
    tunnel_subdomain: Option<String>,
}

impl FileConfig {
    /// Load config from mcpr.toml, searching current dir then parent dirs.
    fn load() -> (Self, Option<std::path::PathBuf>) {
        let mut dir = std::env::current_dir().ok();
        while let Some(d) = dir {
            let path = d.join(CONFIG_FILE);
            if path.exists()
                && let Ok(contents) = std::fs::read_to_string(&path)
            {
                match toml::from_str::<FileConfig>(&contents) {
                    Ok(config) => {
                        eprintln!(
                            "  {} loaded {}",
                            colored::Colorize::dimmed("config"),
                            path.display()
                        );
                        return (config, Some(path));
                    }
                    Err(e) => {
                        eprintln!(
                            "  {}: failed to parse {}: {}",
                            colored::Colorize::yellow("warn"),
                            path.display(),
                            e
                        );
                    }
                }
            }
            dir = d.parent().map(|p| p.to_path_buf());
        }
        (FileConfig::default(), None)
    }

    /// Resolve relay domain: [relay].domain > relay_domain (legacy)
    fn relay_domain(&self) -> Option<String> {
        self.relay.domain.clone().or(self.relay_domain.clone())
    }

    /// Resolve tunnel relay URL: [tunnel].relay_url > relay_url (legacy)
    fn tunnel_relay_url(&self) -> Option<String> {
        self.tunnel.relay_url.clone().or(self.relay_url.clone())
    }

    /// Resolve tunnel token: [tunnel].token > tunnel_token (legacy)
    fn tunnel_token(&self) -> Option<String> {
        self.tunnel.token.clone().or(self.tunnel_token.clone())
    }

    /// Resolve tunnel subdomain: [tunnel].subdomain > tunnel_subdomain (legacy)
    fn tunnel_subdomain(&self) -> Option<String> {
        self.tunnel
            .subdomain
            .clone()
            .or(self.tunnel_subdomain.clone())
    }

    /// Is relay mode via config file: mode = "relay"
    fn is_relay(&self) -> bool {
        self.mode.as_deref() == Some("relay")
    }
}

// ── Gateway config ──────────────────────────────────────────────────────

/// Resolved configuration for gateway (proxy) mode.
pub struct GatewayConfig {
    pub mcp: Option<String>,
    pub widgets: Option<String>,
    pub port: Option<u16>,
    pub csp_domains: Vec<String>,
    pub csp_mode: CspMode,
    pub relay_url: Option<String>,
    pub tunnel_token: Option<String>,
    pub tunnel_subdomain: Option<String>,
    pub tunnel_anonymous: bool,
    pub no_tunnel: bool,
    pub config_path: Option<std::path::PathBuf>,
    pub max_request_body_size: Option<usize>,
    pub max_response_body_size: Option<usize>,
    pub max_concurrent_upstream: Option<usize>,
    pub connect_timeout: Option<u64>,
    pub request_timeout: Option<u64>,
    pub log_file: bool,
    pub log_dir: Option<String>,
    pub log_rotation: Option<String>,
    pub events: bool,
}

impl GatewayConfig {
    /// Resolve tunnel identity from config.
    /// Token and subdomain are independent: token is for auth, subdomain is a preference.
    /// Returns (token, desired_subdomain).
    pub fn resolve_tunnel_identity(
        tunnel_subdomain: Option<String>,
        tunnel_token: Option<String>,
        generate_token: impl FnOnce() -> String,
    ) -> (String, Option<String>) {
        let token = tunnel_token.unwrap_or_else(generate_token);
        (token, tunnel_subdomain)
    }

    /// Append tunnel token to the config file so the URL persists across restarts.
    pub fn save_tunnel_token(path: &std::path::Path, token: &str) {
        match std::fs::read_to_string(path) {
            Ok(contents) => {
                // Check for new [tunnel] table format first
                if contents.contains("[tunnel]") {
                    if contents.contains("token =") || contents.contains("token=") {
                        return; // already set
                    }
                    // Insert token under [tunnel] section
                    let new_contents =
                        contents.replacen("[tunnel]", &format!("[tunnel]\ntoken = \"{token}\""), 1);
                    if let Err(e) = std::fs::write(path, new_contents) {
                        eprintln!(
                            "  {}: failed to save tunnel token to {}: {}",
                            colored::Colorize::yellow("warn"),
                            path.display(),
                            e
                        );
                    } else {
                        eprintln!(
                            "  {} saved tunnel token to {}",
                            colored::Colorize::dimmed("config"),
                            path.display()
                        );
                    }
                    return;
                }

                // Legacy flat format
                if contents.contains("# tunnel_token") {
                    let new_contents = contents.replacen(
                        &contents
                            .lines()
                            .find(|l| l.contains("# tunnel_token"))
                            .unwrap_or("# tunnel_token = \"\"")
                            .to_string(),
                        &format!("tunnel_token = \"{token}\""),
                        1,
                    );
                    if let Err(e) = std::fs::write(path, new_contents) {
                        eprintln!(
                            "  {}: failed to save tunnel_token to {}: {}",
                            colored::Colorize::yellow("warn"),
                            path.display(),
                            e
                        );
                    } else {
                        eprintln!(
                            "  {} saved tunnel_token to {}",
                            colored::Colorize::dimmed("config"),
                            path.display()
                        );
                    }
                } else if !contents.contains("tunnel_token") {
                    let new_contents =
                        format!("{}\ntunnel_token = \"{token}\"\n", contents.trim_end());
                    if let Err(e) = std::fs::write(path, new_contents) {
                        eprintln!(
                            "  {}: failed to save tunnel_token to {}: {}",
                            colored::Colorize::yellow("warn"),
                            path.display(),
                            e
                        );
                    } else {
                        eprintln!(
                            "  {} saved tunnel_token to {}",
                            colored::Colorize::dimmed("config"),
                            path.display()
                        );
                    }
                }
            }
            Err(e) => {
                eprintln!(
                    "  {}: failed to read {}: {}",
                    colored::Colorize::yellow("warn"),
                    path.display(),
                    e
                );
            }
        }
    }

    /// Save both tunnel token and subdomain to the config file.
    pub fn save_tunnel_config(
        path: &std::path::Path,
        token: &str,
        subdomain: &str,
        anonymous: bool,
    ) {
        match std::fs::read_to_string(path) {
            Ok(contents) => {
                let mut new_contents = contents.clone();

                // Check for uncommented [tunnel] section and keys
                let has_tunnel_section = new_contents.lines().any(|l| l.trim() == "[tunnel]");
                let has_token = new_contents.lines().any(|l| {
                    let t = l.trim();
                    !t.starts_with('#') && (t.starts_with("token =") || t.starts_with("token="))
                });
                let has_subdomain = new_contents.lines().any(|l| {
                    let t = l.trim();
                    !t.starts_with('#')
                        && (t.starts_with("subdomain =") || t.starts_with("subdomain="))
                });
                let has_anonymous = new_contents.lines().any(|l| {
                    let t = l.trim();
                    !t.starts_with('#')
                        && (t.starts_with("anonymous =") || t.starts_with("anonymous="))
                });

                if has_tunnel_section {
                    // Insert under existing [tunnel] section if keys are missing
                    let mut insert = String::new();
                    if !has_token {
                        insert.push_str(&format!("token = \"{token}\"\n"));
                    }
                    if !has_subdomain {
                        insert.push_str(&format!("subdomain = \"{subdomain}\"\n"));
                    }
                    if anonymous && !has_anonymous {
                        insert.push_str("anonymous = true\n");
                    }
                    if !insert.is_empty() {
                        new_contents =
                            new_contents.replacen("[tunnel]", &format!("[tunnel]\n{insert}"), 1);
                    }
                } else {
                    // No [tunnel] section — append one
                    let anon_line = if anonymous { "anonymous = true\n" } else { "" };
                    new_contents = format!(
                        "{}\n\n[tunnel]\ntoken = \"{token}\"\nsubdomain = \"{subdomain}\"\n{anon_line}",
                        new_contents.trim_end()
                    );
                }

                if let Err(e) = std::fs::write(path, &new_contents) {
                    eprintln!(
                        "  {}: failed to save tunnel config to {}: {}",
                        colored::Colorize::yellow("warn"),
                        path.display(),
                        e
                    );
                } else {
                    eprintln!(
                        "  {} saved tunnel config to {}",
                        colored::Colorize::dimmed("config"),
                        path.display()
                    );
                }
            }
            Err(e) => {
                eprintln!(
                    "  {}: failed to read {}: {}",
                    colored::Colorize::yellow("warn"),
                    path.display(),
                    e
                );
            }
        }
    }
}

// ── Load + dispatch ─────────────────────────────────────────────────────

/// Parse CLI args, load config file, and return the resolved mode.
pub fn load() -> Mode {
    let cli = Cli::parse();
    let (file, config_path) = FileConfig::load();

    if cli.relay || file.is_relay() {
        load_relay(cli, file)
    } else {
        load_gateway(cli, file, config_path)
    }
}

fn load_relay(cli: Cli, file: FileConfig) -> Mode {
    let port = cli
        .port
        .or(file.port)
        .expect("port is required for relay mode (--port or port in mcpr.toml)");
    let relay_domain = cli.relay_domain.or(file.relay_domain()).expect(
        "relay domain is required for relay mode (--relay-domain or [relay].domain in mcpr.toml)",
    );

    let tokens = file
        .relay
        .tokens
        .into_iter()
        .map(|e| (e.token, e.subdomains))
        .collect();

    Mode::Relay(RelayConfig {
        port,
        relay_domain,
        auth_provider: cli.auth_provider.or(file.relay.auth_provider),
        auth_provider_secret: cli.auth_provider_secret.or(file.relay.auth_provider_secret),
        tokens,
        max_request_body_size: file.max_request_body_size,
        max_response_body_size: file.max_response_body_size,
    })
}

fn load_gateway(cli: Cli, file: FileConfig, config_path: Option<std::path::PathBuf>) -> Mode {
    let tunnel_relay_url = file.tunnel_relay_url();
    let tunnel_token = file.tunnel_token();
    let tunnel_subdomain = file.tunnel_subdomain();
    let tunnel_anonymous = file.tunnel.anonymous;

    // Detect CLI overrides that differ from the config file
    if let Some(path) = &config_path {
        let mut diffs: Vec<(&str, &str, Option<&str>)> = Vec::new(); // (key, cli_val, file_val)

        if let Some(ref cli_mcp) = cli.mcp
            && file.mcp.as_deref() != Some(cli_mcp)
        {
            diffs.push(("mcp", cli_mcp, file.mcp.as_deref()));
        }
        if let Some(ref cli_widgets) = cli.widgets
            && file.widgets.as_deref() != Some(cli_widgets)
        {
            diffs.push(("widgets", cli_widgets, file.widgets.as_deref()));
        }
        if let Some(cli_port) = cli.port {
            let cli_port_str = cli_port.to_string();
            let file_port_str = file.port.map(|p| p.to_string());
            if file.port != Some(cli_port) {
                // Can't borrow temporary — handle port separately below
                drop(cli_port_str);
                drop(file_port_str);
            }
        }

        // Collect port diff separately since it needs owned strings
        let port_diff = cli.port.and_then(|cp| {
            if file.port != Some(cp) {
                Some((cp, file.port))
            } else {
                None
            }
        });

        if !diffs.is_empty() || port_diff.is_some() {
            eprintln!(
                "\n  {} CLI args differ from {}:",
                colored::Colorize::yellow("!"),
                path.display()
            );
            for (key, cli_val, file_val) in &diffs {
                match file_val {
                    Some(fv) => eprintln!(
                        "    {} {} → {}",
                        colored::Colorize::bold(colored::Colorize::white(*key)),
                        colored::Colorize::dimmed(*fv),
                        colored::Colorize::green(*cli_val)
                    ),
                    None => eprintln!(
                        "    {} (unset) → {}",
                        colored::Colorize::bold(colored::Colorize::white(*key)),
                        colored::Colorize::green(*cli_val)
                    ),
                }
            }
            if let Some((cp, fp)) = &port_diff {
                let cp_str = cp.to_string();
                match fp {
                    Some(fv) => {
                        let fv_str = fv.to_string();
                        eprintln!(
                            "    {} {} → {}",
                            colored::Colorize::bold(colored::Colorize::white("port")),
                            colored::Colorize::dimmed(fv_str.as_str()),
                            colored::Colorize::green(cp_str.as_str())
                        );
                    }
                    None => eprintln!(
                        "    {} (unset) → {}",
                        colored::Colorize::bold(colored::Colorize::white("port")),
                        colored::Colorize::green(cp_str.as_str())
                    ),
                }
            }

            eprint!(
                "  {} Save to {}? [y/N] ",
                colored::Colorize::cyan("?"),
                path.display()
            );
            let mut input = String::new();
            if std::io::stdin().read_line(&mut input).is_ok()
                && input.trim().eq_ignore_ascii_case("y")
            {
                save_cli_overrides(path, &diffs, &port_diff);
            }
            eprintln!();
        }
    }

    let csp_domains = if cli.csp_domains.is_empty() {
        file.csp.domains
    } else {
        cli.csp_domains
    };

    let csp_mode = match cli.csp_mode.as_deref().or(file.csp.mode.as_deref()) {
        Some(m) => parse_csp_mode(m),
        None => CspMode::default(),
    };

    Mode::Gateway(GatewayConfig {
        mcp: cli.mcp.or(file.mcp),
        widgets: cli.widgets.or(file.widgets),
        port: cli.port.or(file.port),
        csp_domains,
        csp_mode,
        relay_url: Some(
            cli.relay_url
                .or(tunnel_relay_url)
                .unwrap_or_else(|| "https://tunnel.mcpr.app".to_string()),
        ),
        tunnel_token,
        tunnel_subdomain,
        tunnel_anonymous,
        no_tunnel: cli.no_tunnel || file.no_tunnel,
        config_path,
        max_request_body_size: file.max_request_body_size,
        max_response_body_size: file.max_response_body_size,
        max_concurrent_upstream: file.max_concurrent_upstream,
        connect_timeout: file.connect_timeout,
        request_timeout: file.request_timeout,
        log_file: file.logging.file,
        log_dir: file.logging.dir,
        log_rotation: file.logging.rotation,
        events: cli.events || file.events.enabled,
    })
}

/// Update the TOML config file with CLI overrides.
fn save_cli_overrides(
    path: &std::path::Path,
    diffs: &[(&str, &str, Option<&str>)],
    port_diff: &Option<(u16, Option<u16>)>,
) {
    let contents = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "  {}: failed to read {}: {}",
                colored::Colorize::yellow("warn"),
                path.display(),
                e
            );
            return;
        }
    };

    let mut new_contents = contents.clone();

    for (key, cli_val, file_val) in diffs {
        if file_val.is_some() {
            // Replace existing key = "old_value" with key = "new_value"
            // Match key = "..." or key = '...' patterns at the start of a line
            if let Some(line) = new_contents
                .lines()
                .find(|l| {
                    let trimmed = l.trim();
                    trimmed.starts_with(&format!("{key} ="))
                        || trimmed.starts_with(&format!("{key}="))
                })
                .map(|l| l.to_string())
            {
                new_contents = new_contents.replacen(&line, &format!("{key} = \"{cli_val}\""), 1);
            }
        } else {
            // Key doesn't exist — append before first section or at end
            let insert_line = format!("{key} = \"{cli_val}\"");
            if let Some(pos) = new_contents.find("\n[") {
                new_contents.insert_str(pos, &format!("\n{insert_line}"));
            } else {
                new_contents = format!("{}\n{insert_line}\n", new_contents.trim_end());
            }
        }
    }

    if let Some((cli_port, file_port)) = port_diff {
        if file_port.is_some() {
            if let Some(line) = new_contents
                .lines()
                .find(|l| {
                    let trimmed = l.trim();
                    trimmed.starts_with("port =") || trimmed.starts_with("port=")
                })
                .map(|l| l.to_string())
            {
                new_contents = new_contents.replacen(&line, &format!("port = {cli_port}"), 1);
            }
        } else {
            let insert_line = format!("port = {cli_port}");
            if let Some(pos) = new_contents.find("\n[") {
                new_contents.insert_str(pos, &format!("\n{insert_line}"));
            } else {
                new_contents = format!("{}\n{insert_line}\n", new_contents.trim_end());
            }
        }
    }

    match std::fs::write(path, &new_contents) {
        Ok(_) => eprintln!(
            "  {} saved to {}",
            colored::Colorize::dimmed("config"),
            path.display()
        ),
        Err(e) => eprintln!(
            "  {}: failed to write {}: {}",
            colored::Colorize::yellow("warn"),
            path.display(),
            e
        ),
    }
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subdomain_and_token_are_independent() {
        let (token, sub) = GatewayConfig::resolve_tunnel_identity(
            Some("myapp".into()),
            Some("mcpr_secret_token_123".into()),
            || panic!("should not generate"),
        );
        assert_eq!(token, "mcpr_secret_token_123");
        assert_eq!(sub.as_deref(), Some("myapp"));
    }

    #[test]
    fn subdomain_without_token_generates() {
        let (token, sub) =
            GatewayConfig::resolve_tunnel_identity(Some("myapp".into()), None, || {
                "generated-uuid".into()
            });
        assert_eq!(token, "generated-uuid");
        assert_eq!(sub.as_deref(), Some("myapp"));
    }

    #[test]
    fn no_subdomain_uses_token() {
        let (token, sub) =
            GatewayConfig::resolve_tunnel_identity(None, Some("my-saved-token".into()), || {
                panic!("should not generate")
            });
        assert_eq!(token, "my-saved-token");
        assert_eq!(sub, None);
    }

    #[test]
    fn no_subdomain_no_token_generates() {
        let (token, sub) =
            GatewayConfig::resolve_tunnel_identity(None, None, || "generated-uuid".into());
        assert_eq!(token, "generated-uuid");
        assert_eq!(sub, None);
    }

    #[test]
    fn save_tunnel_config_creates_section_in_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcpr.toml");
        std::fs::write(&path, "").unwrap();

        GatewayConfig::save_tunnel_config(&path, "tok123", "myapp", false);

        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.contains("[tunnel]"));
        assert!(contents.contains("token = \"tok123\""));
        assert!(contents.contains("subdomain = \"myapp\""));
    }

    #[test]
    fn save_tunnel_config_appends_section_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcpr.toml");
        std::fs::write(&path, "port = 8080\n").unwrap();

        GatewayConfig::save_tunnel_config(&path, "tok456", "demo", false);

        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.contains("port = 8080"));
        assert!(contents.contains("[tunnel]"));
        assert!(contents.contains("token = \"tok456\""));
        assert!(contents.contains("subdomain = \"demo\""));
    }

    #[test]
    fn save_tunnel_config_inserts_under_existing_tunnel_section() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcpr.toml");
        std::fs::write(&path, "[tunnel]\nrelay_url = \"https://tunnel.mcpr.app\"\n").unwrap();

        GatewayConfig::save_tunnel_config(&path, "tok789", "example", false);

        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.contains("relay_url = \"https://tunnel.mcpr.app\""));
        assert!(contents.contains("token = \"tok789\""));
        assert!(contents.contains("subdomain = \"example\""));
    }

    #[test]
    fn save_tunnel_config_does_not_duplicate_existing_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcpr.toml");
        std::fs::write(
            &path,
            "[tunnel]\ntoken = \"existing\"\nsubdomain = \"taken\"\n",
        )
        .unwrap();

        GatewayConfig::save_tunnel_config(&path, "new-tok", "new-sub", false);

        let contents = std::fs::read_to_string(&path).unwrap();
        // Original values should be preserved, not duplicated
        assert_eq!(contents.matches("token =").count(), 1);
        assert_eq!(contents.matches("subdomain =").count(), 1);
        assert!(contents.contains("token = \"existing\""));
        assert!(contents.contains("subdomain = \"taken\""));
    }

    #[test]
    fn save_tunnel_config_fails_gracefully_for_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nonexistent.toml");

        // Should not panic — just prints a warning
        GatewayConfig::save_tunnel_config(&path, "tok", "sub", false);

        assert!(!path.exists());
    }

    #[test]
    fn max_request_body_size_parses_from_toml() {
        let toml_str = r#"
            mcp = "http://localhost:9000"
            port = 8080
            max_request_body_size = 10485760
        "#;
        let config: FileConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.max_request_body_size, Some(10_485_760));
    }

    #[test]
    fn max_response_body_size_parses_from_toml() {
        let toml_str = r#"
            mcp = "http://localhost:9000"
            port = 8080
            max_response_body_size = 20971520
        "#;
        let config: FileConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.max_response_body_size, Some(20_971_520));
    }

    #[test]
    fn body_size_defaults_to_none() {
        let toml_str = r#"
            mcp = "http://localhost:9000"
            port = 8080
        "#;
        let config: FileConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.max_request_body_size, None);
        assert_eq!(config.max_response_body_size, None);
        assert_eq!(config.max_concurrent_upstream, None);
    }

    #[test]
    fn max_concurrent_upstream_parses_from_toml() {
        let toml_str = r#"
            mcp = "http://localhost:9000"
            port = 8080
            max_concurrent_upstream = 50
        "#;
        let config: FileConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.max_concurrent_upstream, Some(50));
    }
}
