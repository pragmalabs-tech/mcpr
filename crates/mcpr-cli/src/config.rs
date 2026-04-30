use clap::{Parser, Subcommand};

pub use mcpr_core::proxy2::csp::CspConfig;
use mcpr_core::proxy2::proxy_config::{FileCspConfig, parse_mode};
use mcpr_tunnel::RelayConfig;

const CONFIG_FILE: &str = "mcpr.toml";

// ── Run mode ────────────────────────────────────────────────────────────

/// Top-level mode: either run as a relay server or as the gateway proxy.
#[allow(dead_code)]
pub enum Mode {
    Relay(RelayConfig),
    Gateway(Box<GatewayConfig>),
}

/// Result of parsing CLI args — either a subcommand action or the run mode.
pub enum CliAction {
    Validate(ValidateArgs),
    Version,
    /// Update mcpr to the latest version.
    Update,
    /// Read-only query against the store — no server needed.
    Proxy(ProxyCommand),
    /// Run a proxy in the foreground. The launching process owns the PID
    /// (systemd, Docker, terminal, Node `child_process.spawn`); SIGTERM
    /// drains gracefully.
    ProxyRun {
        mode: Mode,
        /// Raw TOML content for config snapshot.
        config_content: String,
        /// Absolute path to the original config file.
        config_path: String,
    },
    /// Interactive setup flow (needs async for cloud API calls).
    ProxySetup {
        cloud_url: String,
        output: Option<String>,
    },
    /// Store maintenance commands.
    Store(StoreCommand),
    /// Relay subcommands (stop, status — no config needed).
    Relay(RelayCommand),
    /// Run the relay server in the foreground.
    RelayRun {
        relay_config: RelayConfig,
        /// Absolute path to the original config file.
        config_path: String,
    },
}

// ── CLI args ────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(
    name = "mcpr",
    version,
    about = "A proxy for MCP Apps/Servers — routes JSON-RPC, observes traffic, authenticates, and secures MCP."
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
    /// Update mcpr to the latest version
    Update,
    /// Run and manage proxies (run, stop, list, delete, setup)
    Proxy(ProxyArgs),
    /// Storage maintenance (stats, vacuum)
    Store(StoreArgs),
    /// Relay server lifecycle (run, stop, restart, status)
    Relay(RelayArgs),
}

// ── Proxy subcommands ────────────────────────────────────────────────

#[derive(Parser)]
pub struct ProxyArgs {
    #[command(subcommand)]
    pub command: ProxyCommand,
}

/// Proxy lifecycle commands.
#[derive(Subcommand, Clone)]
pub enum ProxyCommand {
    /// Run a proxy in the background from a config file
    Run(ProxyRunArgs),
    /// Stop a running proxy (or all proxies with --all)
    Stop(ProxyStopArgs),
    /// List all known proxies and their status
    List(ProxyListArgs),
    /// Delete a stopped proxy — removes its on-disk state (must be stopped first)
    Delete(ProxyDeleteArgs),
    /// Interactive setup — authenticate, pick project, generate config
    Setup(ProxySetupArgs),
}

// ── Proxy lifecycle args ──────────────────────────────────────────────

/// Arguments for `mcpr proxy run [config]`.
#[derive(Parser, Clone)]
pub struct ProxyRunArgs {
    /// Config file path (default: mcpr.toml)
    pub config: Option<String>,
}

/// Arguments for `mcpr proxy stop [name]`.
#[derive(Parser, Clone)]
pub struct ProxyStopArgs {
    /// Proxy name to stop
    pub name: Option<String>,

    /// Stop all running proxies
    #[arg(long)]
    pub all: bool,
}

/// Arguments for `mcpr proxy list`.
#[derive(Parser, Clone)]
pub struct ProxyListArgs {
    /// Output as JSON (one object per proxy)
    #[arg(long)]
    pub json: bool,
}

/// Arguments for `mcpr proxy delete <name>`.
#[derive(Parser, Clone)]
pub struct ProxyDeleteArgs {
    /// Proxy name to delete
    pub name: String,

    /// Skip the confirmation prompt
    #[arg(long, short = 'y')]
    pub yes: bool,
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

// ── Relay subcommands ─────────────────────────────────────────────────

#[derive(Parser)]
pub struct RelayArgs {
    #[command(subcommand)]
    pub command: RelayCommand,
}

/// Relay server lifecycle commands.
#[derive(Subcommand, Clone)]
pub enum RelayCommand {
    /// Run relay server in the foreground
    Run(RelayRunArgs),
    /// Stop the running relay server (SIGTERM via lockfile)
    Stop,
    /// Show relay server status
    Status,
}

/// Arguments for `mcpr relay run`.
#[derive(Parser, Clone)]
pub struct RelayRunArgs {
    /// Config file path
    pub config: Option<String>,
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
    /// Enable tunnel for a public URL. Default: false (proxy-only mode).
    enabled: bool,
    relay_url: Option<String>,
    token: Option<String>,
    subdomain: Option<String>,
}

/// `[events]` table in config file
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
    mode: Option<String>, // "relay" | "gateway" (default)

    // -- Gateway --
    mcp: Option<String>,
    csp: FileCspConfig,

    // -- Relay --
    relay: FileRelayConfig,

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

    /// Is relay mode via config file: mode = "relay"
    fn is_relay(&self) -> bool {
        self.mode.as_deref() == Some("relay")
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
    pub cloud_token: Option<String>,
    pub cloud_server: Option<String>,
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
        Some(Commands::Update) => CliAction::Update,
        Some(Commands::Proxy(ProxyArgs {
            command: ProxyCommand::Run(run_args),
        })) => {
            // `mcpr proxy run` loads config; foreground by default so a
            // parent process (systemd, Node) supervises the PID directly.
            let explicit_path = run_args.config.as_deref().or(cli.config.as_deref());
            let (file, cfg_path) = FileConfig::load(explicit_path);
            let config_content = cfg_path
                .as_ref()
                .map(|p| std::fs::read_to_string(p).unwrap_or_default())
                .unwrap_or_default();
            let config_path_str = cfg_path
                .as_ref()
                .and_then(|p| p.canonicalize().ok())
                .map(|p| p.display().to_string())
                .unwrap_or_default();

            let runtime = RuntimeOptions {
                drain_timeout: file.drain_timeout.unwrap_or(30),
                admin_bind: file
                    .admin_bind
                    .clone()
                    .unwrap_or_else(|| "127.0.0.1:9901".to_string()),
            };

            let mode = if file.is_relay() {
                load_relay(file, runtime)
            } else {
                load_gateway(file, cfg_path, runtime)
            };

            CliAction::ProxyRun {
                mode,
                config_content,
                config_path: config_path_str,
            }
        }
        Some(Commands::Proxy(ProxyArgs {
            command: ProxyCommand::Setup(setup_args),
        })) => CliAction::ProxySetup {
            cloud_url: setup_args.cloud_url,
            output: setup_args.output,
        },
        Some(Commands::Proxy(args)) => CliAction::Proxy(args.command),
        Some(Commands::Store(args)) => CliAction::Store(args.command),
        Some(Commands::Relay(RelayArgs {
            command: RelayCommand::Run(run_args),
        })) => load_relay_run(run_args, cli.config.as_deref()),
        Some(Commands::Relay(args)) => CliAction::Relay(args.command),
        // No subcommand — print help.
        None => {
            use clap::CommandFactory;
            let _ = Cli::command().print_help();
            eprintln!();
            std::process::exit(2);
        }
    }
}

/// Load config for `mcpr relay run`.
/// The `mode = "relay"` field is not required — it is implicit from the command.
fn load_relay_run(args: RelayRunArgs, global_config: Option<&str>) -> CliAction {
    let explicit_path = args.config.as_deref().or(global_config);
    let (file, cfg_path) = FileConfig::load(explicit_path);
    let config_path_str = cfg_path
        .as_ref()
        .and_then(|p| p.canonicalize().ok())
        .map(|p| p.display().to_string())
        .unwrap_or_default();

    let port = file
        .port
        .expect("port is required for relay mode in config");
    let relay_domain = file
        .relay
        .domain
        .expect("relay.domain is required for relay mode in config");

    let tokens = file
        .relay
        .tokens
        .into_iter()
        .map(|e| (e.token, e.subdomains))
        .collect();

    let relay_config = RelayConfig {
        port,
        relay_domain,
        auth_provider: file.relay.auth_provider,
        auth_provider_secret: file.relay.auth_provider_secret,
        tokens,
        max_request_body_size: file.max_request_body_size,
        max_response_body_size: file.max_response_body_size,
    };

    CliAction::RelayRun {
        relay_config,
        config_path: config_path_str,
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
            // Check required fields for gateway mode
            if !config.is_relay() && config.mcp.is_none() {
                issues.push((
                    "warn",
                    "no 'mcp' URL set — required for gateway mode".to_string(),
                ));
            }

            // Check relay mode requirements
            if config.is_relay() {
                if config.port.is_none() {
                    issues.push(("error", "'port' is required for relay mode".to_string()));
                }
                if config.relay.domain.is_none() {
                    issues.push((
                        "error",
                        "'relay.domain' is required for relay mode".to_string(),
                    ));
                }
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

            // Legacy `csp.mode` + `csp.domains` flat shape: valid values are
            // `extend` and `replace`. `override` is the old spelling — accept
            // it but warn so operators migrate.
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

            // Validate per-directive modes and widget-scoped overrides.
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

fn load_relay(file: FileConfig, _runtime: RuntimeOptions) -> Mode {
    let port = file
        .port
        .expect("port is required for relay mode in mcpr.toml");
    let relay_domain = file
        .relay
        .domain
        .expect("relay.domain is required for relay mode in mcpr.toml");

    let tokens = file
        .relay
        .tokens
        .into_iter()
        .map(|e| (e.token, e.subdomains))
        .collect();

    Mode::Relay(RelayConfig {
        port,
        relay_domain,
        auth_provider: file.relay.auth_provider,
        auth_provider_secret: file.relay.auth_provider_secret,
        tokens,
        max_request_body_size: file.max_request_body_size,
        max_response_body_size: file.max_response_body_size,
    })
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
) -> Mode {
    let name = resolve_proxy_name(file.name.as_deref(), config_path.as_deref());
    let csp = file.csp.into_runtime();

    Mode::Gateway(Box::new(GatewayConfig {
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
        cloud_token: file.cloud.token,
        cloud_server: file.cloud.server,
        cloud_endpoint: file.cloud.endpoint,
        cloud_batch_size: file.cloud.batch_size,
        cloud_flush_interval_ms: file.cloud.flush_interval_ms,
        runtime,
    }))
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
        // An operator partially migrated: they have the new connectDomains
        // block plus a leftover legacy mode+domains. The new block wins; the
        // legacy shape only fills directives the operator hasn't migrated yet.
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
        // resourceDomains still comes from the legacy fallback.
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

    // ── Relay config parsing ──────────────────────────────────────────

    #[test]
    fn file_config__relay_mode_detected() {
        let toml_str = r#"
            mode = "relay"
            port = 8080
            [relay]
            domain = "tunnel.example.com"
        "#;
        let config: FileConfig = toml::from_str(toml_str).unwrap();
        assert!(config.is_relay());
    }

    #[test]
    fn file_config__gateway_mode_by_default() {
        let toml_str = r#"
            mcp = "http://localhost:9000"
            port = 3000
        "#;
        let config: FileConfig = toml::from_str(toml_str).unwrap();
        assert!(!config.is_relay());
    }

    #[test]
    fn file_config__relay_domain_parsed() {
        let toml_str = r#"
            mode = "relay"
            port = 8080
            [relay]
            domain = "tunnel.example.com"
        "#;
        let config: FileConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.relay.domain.as_deref(), Some("tunnel.example.com"));
    }

    #[test]
    fn file_config__relay_static_tokens() {
        let toml_str = r#"
            mode = "relay"
            port = 8080
            [relay]
            domain = "tunnel.example.com"
            [[relay.tokens]]
            token = "tok_abc"
            subdomains = ["myapp", "myapp-*"]
            [[relay.tokens]]
            token = "tok_xyz"
            subdomains = ["other"]
        "#;
        let config: FileConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.relay.tokens.len(), 2);
        assert_eq!(config.relay.tokens[0].token, "tok_abc");
        assert_eq!(config.relay.tokens[0].subdomains, vec!["myapp", "myapp-*"]);
        assert_eq!(config.relay.tokens[1].token, "tok_xyz");
        assert_eq!(config.relay.tokens[1].subdomains, vec!["other"]);
    }

    #[test]
    fn file_config__relay_auth_provider() {
        let toml_str = r#"
            mode = "relay"
            port = 8080
            [relay]
            domain = "tunnel.example.com"
            auth_provider = "https://auth.example.com"
            auth_provider_secret = "secret123"
        "#;
        let config: FileConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(
            config.relay.auth_provider.as_deref(),
            Some("https://auth.example.com")
        );
        assert_eq!(
            config.relay.auth_provider_secret.as_deref(),
            Some("secret123")
        );
    }

    #[test]
    fn file_config__relay_defaults_empty() {
        let toml_str = r#"
            mcp = "http://localhost:9000"
        "#;
        let config: FileConfig = toml::from_str(toml_str).unwrap();
        assert!(config.relay.domain.is_none());
        assert!(config.relay.auth_provider.is_none());
        assert!(config.relay.auth_provider_secret.is_none());
        assert!(config.relay.tokens.is_empty());
    }

    #[test]
    fn file_config__relay_body_size_limits() {
        let toml_str = r#"
            mode = "relay"
            port = 8080
            max_request_body_size = 1048576
            max_response_body_size = 2097152
            [relay]
            domain = "tunnel.example.com"
        "#;
        let config: FileConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.max_request_body_size, Some(1_048_576));
        assert_eq!(config.max_response_body_size, Some(2_097_152));
    }

    // ── load_relay ────────────────────────────────────────────────────

    #[test]
    fn load_relay__builds_relay_config() {
        let toml_str = r#"
            mode = "relay"
            port = 9090
            [relay]
            domain = "tunnel.test"
            [[relay.tokens]]
            token = "tok_a"
            subdomains = ["app1"]
        "#;
        let file: FileConfig = toml::from_str(toml_str).unwrap();
        let runtime = RuntimeOptions {
            drain_timeout: 30,
            admin_bind: "127.0.0.1:9901".to_string(),
        };
        let mode = load_relay(file, runtime);
        match mode {
            Mode::Relay(cfg) => {
                assert_eq!(cfg.port, 9090);
                assert_eq!(cfg.relay_domain, "tunnel.test");
                assert_eq!(cfg.tokens.len(), 1);
                assert!(cfg.tokens.contains_key("tok_a"));
                assert_eq!(cfg.tokens["tok_a"], vec!["app1"]);
            }
            Mode::Gateway(_) => panic!("expected Mode::Relay"),
        }
    }

    #[test]
    #[should_panic(expected = "port is required")]
    fn load_relay__panics_without_port() {
        let toml_str = r#"
            mode = "relay"
            [relay]
            domain = "tunnel.test"
        "#;
        let file: FileConfig = toml::from_str(toml_str).unwrap();
        let runtime = RuntimeOptions {
            drain_timeout: 30,
            admin_bind: "127.0.0.1:9901".to_string(),
        };
        load_relay(file, runtime);
    }

    #[test]
    #[should_panic(expected = "relay.domain is required")]
    fn load_relay__panics_without_domain() {
        let toml_str = r#"
            mode = "relay"
            port = 8080
            [relay]
        "#;
        let file: FileConfig = toml::from_str(toml_str).unwrap();
        let runtime = RuntimeOptions {
            drain_timeout: 30,
            admin_bind: "127.0.0.1:9901".to_string(),
        };
        load_relay(file, runtime);
    }

    // ── load_relay_run ────────────────────────────────────────────────

    #[test]
    fn load_relay_run__basic() {
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("relay.toml");
        std::fs::write(
            &cfg_path,
            "port = 9090\n[relay]\ndomain = \"tunnel.test\"\n",
        )
        .unwrap();

        let args = RelayRunArgs {
            config: Some(cfg_path.display().to_string()),
        };
        let action = load_relay_run(args, None);
        match action {
            CliAction::RelayRun { relay_config, .. } => {
                assert_eq!(relay_config.port, 9090);
                assert_eq!(relay_config.relay_domain, "tunnel.test");
            }
            _ => panic!("expected CliAction::RelayRun"),
        }
    }

    #[test]
    fn load_relay_run__with_tokens() {
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("relay.toml");
        std::fs::write(
            &cfg_path,
            r#"
port = 8080
[relay]
domain = "tunnel.test"
[[relay.tokens]]
token = "tok_abc"
subdomains = ["myapp"]
"#,
        )
        .unwrap();

        let args = RelayRunArgs {
            config: Some(cfg_path.display().to_string()),
        };
        let action = load_relay_run(args, None);
        match action {
            CliAction::RelayRun { relay_config, .. } => {
                assert_eq!(relay_config.tokens.len(), 1);
                assert!(relay_config.tokens.contains_key("tok_abc"));
            }
            _ => panic!("expected CliAction::RelayRun"),
        }
    }

    #[test]
    fn load_relay_run__with_auth_provider() {
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("relay.toml");
        std::fs::write(
            &cfg_path,
            r#"
port = 8080
[relay]
domain = "tunnel.test"
auth_provider = "https://auth.example.com"
auth_provider_secret = "secret123"
"#,
        )
        .unwrap();

        let args = RelayRunArgs {
            config: Some(cfg_path.display().to_string()),
        };
        let action = load_relay_run(args, None);
        match action {
            CliAction::RelayRun { relay_config, .. } => {
                assert_eq!(
                    relay_config.auth_provider.as_deref(),
                    Some("https://auth.example.com")
                );
                assert_eq!(
                    relay_config.auth_provider_secret.as_deref(),
                    Some("secret123")
                );
            }
            _ => panic!("expected CliAction::RelayRun"),
        }
    }

    #[test]
    #[should_panic(expected = "port is required")]
    fn load_relay_run__panics_without_port() {
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("relay.toml");
        std::fs::write(&cfg_path, "[relay]\ndomain = \"tunnel.test\"\n").unwrap();

        let args = RelayRunArgs {
            config: Some(cfg_path.display().to_string()),
        };
        load_relay_run(args, None);
    }

    #[test]
    #[should_panic(expected = "relay.domain is required")]
    fn load_relay_run__panics_without_domain() {
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("relay.toml");
        std::fs::write(&cfg_path, "port = 8080\n[relay]\n").unwrap();

        let args = RelayRunArgs {
            config: Some(cfg_path.display().to_string()),
        };
        load_relay_run(args, None);
    }
}
