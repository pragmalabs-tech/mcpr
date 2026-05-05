use clap::{Parser, Subcommand};

pub use mcpr_core::proxy2::csp::CspConfig;
use mcpr_core::proxy2::proxy_config::{AuthConfig, FileAuthConfig, FileCspConfig, parse_mode};

const CONFIG_FILE: &str = "mcpr.toml";

/// Result of parsing CLI args - either a subcommand action or the run mode.
pub enum CliAction {
    Validate(ValidateArgs),
    Version,
    /// Run a proxy in the foreground. The launching process owns the PID
    /// (systemd, Docker, terminal, Node `child_process.spawn`); SIGTERM
    /// drains gracefully.
    ProxyRun {
        cfg: Box<GatewayConfig>,
    },
    /// Interactive setup flow (needs async for cloud API calls).
    ProxySetup {
        cloud_url: String,
        output: Option<String>,
    },
    /// Store maintenance commands.
    Store(StoreCommand),
}

// ── CLI args ────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(
    name = "mcpr",
    version,
    about = "A proxy for MCP Apps/Servers - routes JSON-RPC, observes traffic, authenticates, and secures MCP."
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Path to config file (default: ./mcpr.toml)
    #[arg(long, short, global = true)]
    config: Option<String>,
}

#[derive(Subcommand)]
enum Commands {
    /// Validate config file and exit
    Validate(ValidateArgs),
    /// Print version information and exit
    Version,
    /// Run and manage proxies (run, stop, list, delete, setup)
    Proxy(ProxyArgs),
    /// Storage maintenance (stats, vacuum)
    Store(StoreArgs),
}

// ── Proxy subcommands ────────────────────────────────────────────────

#[derive(Parser)]
pub struct ProxyArgs {
    #[command(subcommand)]
    pub command: ProxyCommand,
}

/// Proxy lifecycle commands.
///
/// mcpr is a sidecar primitive in the envoy / pgbouncer mold. The host
/// process supervisor (systemd, Docker, k8s) owns the proxy PID, so
/// listing / stopping / deleting individual proxies is the supervisor's
/// job - not the CLI's.
#[derive(Subcommand, Clone)]
pub enum ProxyCommand {
    /// Run a proxy in the foreground from a config file
    Run(ProxyRunArgs),
    /// Interactive setup - authenticate, pick project, generate config
    Setup(ProxySetupArgs),
}

// ── Proxy lifecycle args ──────────────────────────────────────────────

/// Arguments for `mcpr proxy run [config]`.
#[derive(Parser, Clone)]
pub struct ProxyRunArgs {
    /// Config file path (default: mcpr.toml)
    pub config: Option<String>,
}

/// Arguments for `mcpr proxy setup`.
#[derive(Parser, Clone)]
pub struct ProxySetupArgs {
    /// Cloud API base URL (default: https://api.mcpr.app)
    #[arg(long, default_value = "https://api.mcpr.app")]
    pub cloud_url: String,

    /// Output config path (default: ./mcpr.toml)
    #[arg(long, short)]
    pub output: Option<String>,
}

// ── Store subcommands ──────────────────────────────────────────────────

#[derive(Parser)]
pub struct StoreArgs {
    #[command(subcommand)]
    pub command: StoreCommand,
}

/// Storage maintenance commands.
#[derive(Subcommand, Clone)]
pub enum StoreCommand {
    /// Show database size, row counts, and age of records
    Stats,
    /// Delete old records and reclaim disk space
    Vacuum(StoreVacuumArgs),
}

/// Arguments for `mcpr store vacuum`.
#[derive(Parser, Clone)]
pub struct StoreVacuumArgs {
    /// Delete records older than this duration or date (e.g., 7d, 30d)
    #[arg(long)]
    pub before: String,

    /// Only vacuum records for one proxy
    #[arg(long)]
    pub proxy: Option<String>,

    /// Show what would be deleted without actually deleting
    #[arg(long)]
    pub dry_run: bool,
}

/// Arguments for the `validate` subcommand.
#[derive(Parser, Clone)]
pub struct ValidateArgs {
    /// Config file path to validate
    #[arg(short, long)]
    pub config: Option<String>,
    /// Dump resolved config to stdout after validation
    #[arg(long)]
    pub dump: bool,
}

/// Runtime options extracted from CLI args (used by main.rs).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeOptions {
    pub drain_timeout: u64,
    pub admin_bind: String,
}

// ── TOML config file ────────────────────────────────────────────────────

/// `[tunnel]` table in config file
#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct FileTunnelConfig {
    /// Enable tunnel for a public URL. Default: false (proxy-only mode).
    enabled: bool,
    relay_url: Option<String>,
    token: Option<String>,
    subdomain: Option<String>,
}

/// `[cloud]` table in config file
#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct FileCloudConfig {
    token: Option<String>,
    server: Option<String>,
    endpoint: Option<String>,
    batch_size: Option<usize>,
    flush_interval_ms: Option<u64>,
}

/// Config file format (mcpr.toml)
#[derive(serde::Deserialize, Default)]
#[serde(default)]
struct FileConfig {
    // -- Identity --
    name: Option<String>,

    // -- Shared --
    port: Option<u16>,

    // -- Gateway --
    mcp: Option<String>,
    csp: FileCspConfig,

    // -- OAuth 2.1 protected resource (RFC 9728) --
    auth: Option<FileAuthConfig>,

    // -- Tunnel client --
    tunnel: FileTunnelConfig,

    // -- Cloud sync --
    cloud: FileCloudConfig,

    // -- Runtime --
    drain_timeout: Option<u64>,
    log_format: Option<String>,

    // -- Admin --
    admin_bind: Option<String>,

    max_request_body_size: Option<usize>,
    max_response_body_size: Option<usize>,
    max_concurrent_upstream: Option<usize>,
    connect_timeout: Option<u64>,
    request_timeout: Option<u64>,
}

impl FileConfig {
    /// Load config from a specific path, or ./mcpr.toml by default.
    fn load(explicit_path: Option<&str>) -> (Self, Option<std::path::PathBuf>) {
        let path = match explicit_path {
            Some(p) => std::path::PathBuf::from(p),
            None => std::env::current_dir()
                .unwrap_or_default()
                .join(CONFIG_FILE),
        };

        if !path.exists() {
            if explicit_path.is_some() {
                eprintln!(
                    "  {}: config file not found: {}",
                    colored::Colorize::red("error"),
                    path.display()
                );
                std::process::exit(1);
            }
            return (FileConfig::default(), None);
        }

        match std::fs::read_to_string(&path) {
            Ok(contents) => match toml::from_str::<FileConfig>(&contents) {
                Ok(config) => {
                    eprintln!(
                        "  {} loaded {}",
                        colored::Colorize::dimmed("config"),
                        path.display()
                    );
                    (config, Some(path))
                }
                Err(e) => {
                    eprintln!(
                        "  {}: failed to parse {}: {}",
                        colored::Colorize::red("error"),
                        path.display(),
                        e
                    );
                    std::process::exit(1);
                }
            },
            Err(e) => {
                eprintln!(
                    "  {}: failed to read {}: {}",
                    colored::Colorize::red("error"),
                    path.display(),
                    e
                );
                std::process::exit(1);
            }
        }
    }
}

// ── Gateway config ──────────────────────────────────────────────────────

/// Resolved configuration for gateway (proxy) mode.
#[derive(Clone)]
#[allow(dead_code)]
pub struct GatewayConfig {
    pub name: String,
    pub mcp: Option<String>,
    pub port: Option<u16>,
    pub csp: CspConfig,
    pub relay_url: Option<String>,
    pub tunnel_token: Option<String>,
    pub tunnel_subdomain: Option<String>,
    /// Whether tunnel is enabled. Default: false (proxy-only mode).
    pub tunnel: bool,
    pub max_request_body_size: Option<usize>,
    pub max_response_body_size: Option<usize>,
    pub max_concurrent_upstream: Option<usize>,
    pub connect_timeout: Option<u64>,
    pub request_timeout: Option<u64>,
    pub auth: Option<AuthConfig>,
    pub cloud_token: Option<String>,
    pub cloud_server: Option<String>,
    // Default is https://api.mcpr.app/api/ingest-events
    pub cloud_endpoint: Option<String>,
    pub cloud_batch_size: Option<usize>,
    pub cloud_flush_interval_ms: Option<u64>,
    pub runtime: RuntimeOptions,
}

// ── Load + dispatch ─────────────────────────────────────────────────────

/// Parse CLI args, load config file, and return the resolved action.
pub fn load() -> CliAction {
    let cli = Cli::parse();

    match cli.command {
        Some(Commands::Validate(args)) => CliAction::Validate(args),
        Some(Commands::Version) => CliAction::Version,
        Some(Commands::Proxy(ProxyArgs {
            command: ProxyCommand::Run(run_args),
        })) => {
            let explicit_path = run_args.config.as_deref().or(cli.config.as_deref());
            let (file, cfg_path) = FileConfig::load(explicit_path);

            let runtime = RuntimeOptions {
                drain_timeout: file.drain_timeout.unwrap_or(30),
                admin_bind: file
                    .admin_bind
                    .clone()
                    .unwrap_or_else(|| "127.0.0.1:9901".to_string()),
            };

            let cfg = load_gateway(file, cfg_path, runtime);

            CliAction::ProxyRun { cfg }
        }
        Some(Commands::Proxy(ProxyArgs {
            command: ProxyCommand::Setup(setup_args),
        })) => CliAction::ProxySetup {
            cloud_url: setup_args.cloud_url,
            output: setup_args.output,
        },
        Some(Commands::Store(args)) => CliAction::Store(args.command),
        None => {
            use clap::CommandFactory;
            let _ = Cli::command().print_help();
            eprintln!();
            std::process::exit(2);
        }
    }
}

/// Validate a config file and return a list of (severity, message) tuples.
/// Severity is "error" or "warn".
pub fn validate_config(path: Option<&str>) -> Vec<(&'static str, String)> {
    let mut issues = Vec::new();

    let config_path = path
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap().join(CONFIG_FILE));

    if !config_path.exists() {
        issues.push((
            "error",
            format!("config file not found: {}", config_path.display()),
        ));
        return issues;
    }

    let contents = match std::fs::read_to_string(&config_path) {
        Ok(c) => c,
        Err(e) => {
            issues.push((
                "error",
                format!("cannot read {}: {}", config_path.display(), e),
            ));
            return issues;
        }
    };

    match toml::from_str::<FileConfig>(&contents) {
        Ok(config) => {
            if config.mcp.is_none() {
                issues.push((
                    "warn",
                    "no 'mcp' URL set - required for gateway mode".to_string(),
                ));
            }

            // Validate MCP URL if set
            if let Some(ref mcp) = config.mcp
                && url::Url::parse(mcp).is_err()
            {
                issues.push(("error", format!("invalid MCP URL: {mcp}")));
            }

            // Validate port range
            if let Some(port) = config.port
                && port == 0
            {
                issues.push(("warn", "port 0 will bind to a random port".to_string()));
            }

            if let Some(ref mode) = config.csp.mode {
                match mode.to_lowercase().as_str() {
                    "extend" | "replace" => {}
                    "override" => issues.push((
                        "warn",
                        "csp.mode = \"override\" is deprecated; use \"replace\"".to_string(),
                    )),
                    other => issues.push((
                        "error",
                        format!("invalid csp.mode: {other} (expected: extend, replace)"),
                    )),
                }
            }
            if !config.csp.domains.is_empty() && config.csp.connect_domains.is_none() {
                issues.push((
                    "warn",
                    "csp.domains is deprecated; declare csp.connectDomains and csp.resourceDomains instead"
                        .to_string(),
                ));
            }

            for (name, policy) in [
                ("csp.connectDomains.mode", &config.csp.connect_domains),
                ("csp.resourceDomains.mode", &config.csp.resource_domains),
                ("csp.frameDomains.mode", &config.csp.frame_domains),
            ] {
                if let Some(p) = policy
                    && let Some(m) = p.mode.as_deref()
                    && parse_mode(m).is_none()
                {
                    issues.push((
                        "error",
                        format!("invalid {name}: {m} (expected: extend, replace)"),
                    ));
                }
            }
            for (idx, w) in config.csp.widgets.iter().enumerate() {
                if w.match_pattern.is_empty() {
                    issues.push(("error", format!("csp.widget[{idx}].match is required")));
                }
            }

            if issues.is_empty() {
                issues.push(("ok", format!("config valid: {}", config_path.display())));
            }
        }
        Err(e) => {
            issues.push(("error", format!("invalid TOML: {e}")));
        }
    }

    issues
}

/// Resolve proxy name: explicit name > filename stem > "default".
/// Characters that aren't alphanumeric or '-' are replaced with '-'.
fn resolve_proxy_name(
    explicit_name: Option<&str>,
    config_path: Option<&std::path::Path>,
) -> String {
    let raw = match explicit_name {
        Some(n) => n.to_string(),
        None => config_path
            .and_then(|p| p.file_stem())
            .and_then(|s| s.to_str())
            .map(|s| if s == "mcpr" { "default" } else { s })
            .unwrap_or("default")
            .to_string(),
    };

    raw.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

fn load_gateway(
    file: FileConfig,
    config_path: Option<std::path::PathBuf>,
    runtime: RuntimeOptions,
) -> Box<GatewayConfig> {
    let name = resolve_proxy_name(file.name.as_deref(), config_path.as_deref());
    let csp = file.csp.into_runtime();
    let auth = file.auth.and_then(|a| a.into_runtime(file.port));

    Box::new(GatewayConfig {
        name,
        mcp: file.mcp,
        port: file.port,
        csp,
        relay_url: Some(
            file.tunnel
                .relay_url
                .unwrap_or_else(|| "https://tunnel.mcpr.app".to_string()),
        ),
        tunnel_token: file.tunnel.token,
        tunnel_subdomain: file.tunnel.subdomain,
        tunnel: file.tunnel.enabled,
        max_request_body_size: file.max_request_body_size,
        max_response_body_size: file.max_response_body_size,
        max_concurrent_upstream: file.max_concurrent_upstream,
        connect_timeout: file.connect_timeout,
        request_timeout: file.request_timeout,
        auth,
        cloud_token: file.cloud.token,
        cloud_server: file.cloud.server,
        cloud_endpoint: file.cloud.endpoint,
        cloud_batch_size: file.cloud.batch_size,
        cloud_flush_interval_ms: file.cloud.flush_interval_ms,
        runtime,
    })
}

// ── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;
    use mcpr_core::proxy2::csp::Mode as CspMode;

    #[test]
    fn file_config__max_request_body_size() {
        let toml_str = r#"
            mcp = "http://localhost:9000"
            port = 8080
            max_request_body_size = 10485760
        "#;
        let config: FileConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.max_request_body_size, Some(10_485_760));
    }

    #[test]
    fn file_config__max_response_body_size() {
        let toml_str = r#"
            mcp = "http://localhost:9000"
            port = 8080
            max_response_body_size = 20971520
        "#;
        let config: FileConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.max_response_body_size, Some(20_971_520));
    }

    #[test]
    fn file_config__body_size_defaults_to_none() {
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
    fn file_config__max_concurrent_upstream() {
        let toml_str = r#"
            mcp = "http://localhost:9000"
            port = 8080
            max_concurrent_upstream = 50
        "#;
        let config: FileConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.max_concurrent_upstream, Some(50));
    }

    // ── Cloud config parsing tests ─────────────────────────────────────

    #[test]
    fn cloud_config__parses_all_fields() {
        let toml_str = r#"
            [cloud]
            token = "mcpr_abc123"
            server = "my-proxy"
            endpoint = "https://custom.api/ingest"
            batch_size = 50
            flush_interval_ms = 10000
        "#;
        let config: FileConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.cloud.token.as_deref(), Some("mcpr_abc123"));
        assert_eq!(config.cloud.server.as_deref(), Some("my-proxy"));
        assert_eq!(
            config.cloud.endpoint.as_deref(),
            Some("https://custom.api/ingest")
        );
        assert_eq!(config.cloud.batch_size, Some(50));
        assert_eq!(config.cloud.flush_interval_ms, Some(10000));
    }

    #[test]
    fn cloud_config__defaults_to_none() {
        let toml_str = r#"
            mcp = "http://localhost:9000"
        "#;
        let config: FileConfig = toml::from_str(toml_str).unwrap();
        assert!(config.cloud.token.is_none());
        assert!(config.cloud.server.is_none());
        assert!(config.cloud.endpoint.is_none());
        assert!(config.cloud.batch_size.is_none());
        assert!(config.cloud.flush_interval_ms.is_none());
    }

    #[test]
    fn cloud_config__partial_fields() {
        let toml_str = r#"
            [cloud]
            token = "mcpr_xyz"
        "#;
        let config: FileConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.cloud.token.as_deref(), Some("mcpr_xyz"));
        assert!(config.cloud.server.is_none());
        assert!(config.cloud.endpoint.is_none());
        assert!(config.cloud.batch_size.is_none());
        assert!(config.cloud.flush_interval_ms.is_none());
    }

    #[test]
    fn cloud_config__coexists_with_other_sections() {
        let toml_str = r#"
            mcp = "http://localhost:9000"
            port = 8080

            [cloud]
            token = "mcpr_tok"
            server = "prod-1"

            [tunnel]
            relay_url = "https://tunnel.mcpr.app"
        "#;
        let config: FileConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.cloud.token.as_deref(), Some("mcpr_tok"));
        assert_eq!(config.cloud.server.as_deref(), Some("prod-1"));
        assert_eq!(config.mcp.as_deref(), Some("http://localhost:9000"));
        assert_eq!(
            config.tunnel.relay_url.as_deref(),
            Some("https://tunnel.mcpr.app")
        );
    }

    #[test]
    fn cloud_config__empty_section_uses_defaults() {
        let toml_str = r#"
            [cloud]
        "#;
        let config: FileConfig = toml::from_str(toml_str).unwrap();
        assert!(config.cloud.token.is_none());
        assert!(config.cloud.server.is_none());
    }

    // ── CSP config parsing ─────────────────────────────────────────────

    #[test]
    fn csp_config__canonical_shape_parses() {
        let toml_str = r#"
            [csp.connectDomains]
            domains = ["api.example.com"]
            mode    = "extend"

            [csp.resourceDomains]
            domains = ["cdn.example.com"]
            mode    = "extend"

            [csp.frameDomains]
            domains = []
            mode    = "replace"

            [[csp.widget]]
            match              = "ui://widget/payment*"
            connectDomains     = ["api.stripe.com"]
            connectDomainsMode = "extend"
        "#;
        let file: FileConfig = toml::from_str(toml_str).unwrap();
        let cfg = file.csp.into_runtime();
        assert_eq!(cfg.connect_domains.domains, vec!["api.example.com"]);
        assert_eq!(cfg.connect_domains.mode, CspMode::Extend);
        assert_eq!(cfg.resource_domains.domains, vec!["cdn.example.com"]);
        assert_eq!(cfg.frame_domains.mode, CspMode::Replace);
        assert_eq!(cfg.widgets.len(), 1);
        assert_eq!(cfg.widgets[0].match_pattern, "ui://widget/payment*");
        assert_eq!(cfg.widgets[0].connect_domains, vec!["api.stripe.com"]);
        assert_eq!(cfg.widgets[0].connect_domains_mode, CspMode::Extend);
    }

    #[test]
    fn csp_config__legacy_flat_shape_populates_connect_and_resource() {
        let toml_str = r#"
            [csp]
            mode    = "extend"
            domains = ["api.legacy.com"]
        "#;
        let file: FileConfig = toml::from_str(toml_str).unwrap();
        let cfg = file.csp.into_runtime();
        assert_eq!(cfg.connect_domains.domains, vec!["api.legacy.com"]);
        assert_eq!(cfg.resource_domains.domains, vec!["api.legacy.com"]);
        assert_eq!(cfg.connect_domains.mode, CspMode::Extend);
        assert_eq!(cfg.frame_domains.mode, CspMode::Replace);
    }

    #[test]
    fn csp_config__legacy_override_maps_to_replace() {
        let toml_str = r#"
            [csp]
            mode    = "override"
            domains = ["api.legacy.com"]
        "#;
        let file: FileConfig = toml::from_str(toml_str).unwrap();
        let cfg = file.csp.into_runtime();
        assert_eq!(cfg.connect_domains.mode, CspMode::Replace);
        assert_eq!(cfg.resource_domains.mode, CspMode::Replace);
    }

    #[test]
    fn csp_config__empty_defaults_strict_frames() {
        let file: FileConfig = toml::from_str("").unwrap();
        let cfg = file.csp.into_runtime();
        assert!(cfg.connect_domains.domains.is_empty());
        assert!(cfg.resource_domains.domains.is_empty());
        assert_eq!(cfg.frame_domains.mode, CspMode::Replace);
        assert!(cfg.widgets.is_empty());
        assert!(cfg.domain.is_none());
    }

    #[test]
    fn csp_config__domain_parses() {
        let toml_str = r#"
            [csp]
            domain = "widgets.example.com"
        "#;
        let file: FileConfig = toml::from_str(toml_str).unwrap();
        let cfg = file.csp.into_runtime();
        assert_eq!(cfg.domain.as_deref(), Some("widgets.example.com"));
    }

    #[test]
    fn csp_config__domain_whitespace_is_trimmed_and_empty_ignored() {
        let toml_str = r#"
            [csp]
            domain = "   "
        "#;
        let file: FileConfig = toml::from_str(toml_str).unwrap();
        let cfg = file.csp.into_runtime();
        assert!(cfg.domain.is_none());
    }

    #[test]
    fn csp_config__canonical_overrides_legacy_when_both_present() {
        let toml_str = r#"
            [csp]
            mode    = "extend"
            domains = ["legacy.com"]

            [csp.connectDomains]
            domains = ["new.com"]
            mode    = "replace"
        "#;
        let file: FileConfig = toml::from_str(toml_str).unwrap();
        let cfg = file.csp.into_runtime();
        assert_eq!(cfg.connect_domains.domains, vec!["new.com"]);
        assert_eq!(cfg.connect_domains.mode, CspMode::Replace);
        assert_eq!(cfg.resource_domains.domains, vec!["legacy.com"]);
        assert_eq!(cfg.resource_domains.mode, CspMode::Extend);
    }

    #[test]
    fn parse_mode__accepts_known_values() {
        assert_eq!(parse_mode("extend"), Some(CspMode::Extend));
        assert_eq!(parse_mode("replace"), Some(CspMode::Replace));
        assert_eq!(parse_mode("override"), Some(CspMode::Replace));
        assert_eq!(parse_mode("EXTEND"), Some(CspMode::Extend));
    }

    #[test]
    fn parse_mode__rejects_unknown() {
        assert_eq!(parse_mode(""), None);
        assert_eq!(parse_mode("strict"), None);
        assert_eq!(parse_mode("off"), None);
    }

    // ── Proxy name resolution ──────────────────────────────────────────

    #[test]
    fn resolve_proxy_name__explicit_wins() {
        let name = resolve_proxy_name(
            Some("my-proxy"),
            Some(std::path::Path::new("/tmp/search.toml")),
        );
        assert_eq!(name, "my-proxy");
    }

    #[test]
    fn resolve_proxy_name__from_filename_stem() {
        let name = resolve_proxy_name(None, Some(std::path::Path::new("/tmp/search.toml")));
        assert_eq!(name, "search");
    }

    #[test]
    fn resolve_proxy_name__mcpr_toml_becomes_default() {
        let name = resolve_proxy_name(None, Some(std::path::Path::new("/tmp/mcpr.toml")));
        assert_eq!(name, "default");
    }

    #[test]
    fn resolve_proxy_name__no_config_becomes_default() {
        let name = resolve_proxy_name(None, None);
        assert_eq!(name, "default");
    }

    #[test]
    fn resolve_proxy_name__sanitizes_special_chars() {
        let name = resolve_proxy_name(Some("my proxy!@#$"), None);
        assert_eq!(name, "my-proxy----");
    }

    #[test]
    fn resolve_proxy_name__preserves_hyphens() {
        let name = resolve_proxy_name(Some("search-v2"), None);
        assert_eq!(name, "search-v2");
    }

    #[test]
    fn file_config__name_from_toml() {
        let toml_str = r#"
            name = "email"
            mcp = "http://localhost:9000"
        "#;
        let config: FileConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.name.as_deref(), Some("email"));
    }
}
