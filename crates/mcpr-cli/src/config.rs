use clap::{Parser, Subcommand};

pub use mcpr_proxy::{CspMode, parse_csp_mode};
use mcpr_tunnel::RelayConfig;

const CONFIG_FILE: &str = "mcpr.toml";

// ── Run mode ────────────────────────────────────────────────────────────

/// Top-level mode: either run as a relay server or as the gateway proxy.
pub enum Mode {
    Relay(RelayConfig),
    Gateway(Box<GatewayConfig>),
}

/// Result of parsing CLI args — either a subcommand action or the run mode.
pub enum CliAction {
    Run(Mode),
    /// Start proxy as a background daemon (detached from terminal).
    Start(Mode),
    /// Stop the running daemon via SIGTERM.
    Stop,
    /// Stop + start the daemon.
    Restart(Mode),
    /// Show daemon status (PID, port, uptime).
    Status,
    Validate(ValidateArgs),
    Version,
    /// Read-only query against the store — no server needed.
    Proxy(ProxyCommand),
    /// Store maintenance commands.
    Store(StoreCommand),
}

// ── Log format ──────────────────────────────────────────────────────────

/// Log output format for stderr.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LogFormat {
    #[default]
    Json,
    Pretty,
}

impl std::str::FromStr for LogFormat {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "json" => Ok(LogFormat::Json),
            "pretty" | "text" => Ok(LogFormat::Pretty),
            _ => Err(format!("unknown log format: {s} (expected: json, pretty)")),
        }
    }
}

impl std::fmt::Display for LogFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LogFormat::Json => write!(f, "json"),
            LogFormat::Pretty => write!(f, "pretty"),
        }
    }
}

// ── CLI args ────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(
    name = "mcpr",
    version,
    about = "Open-source proxy for MCP Apps — fixes CSP, handles auth, observes every tool call."
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    // ── Backward-compat: allow flags without subcommand (`mcpr --mcp ...`) ──
    /// Upstream MCP server URL
    #[arg(long, global = true)]
    mcp: Option<String>,

    /// Widget source: URL (proxy mode) or path (static mode)
    #[arg(long, global = true)]
    widgets: Option<String>,

    /// Local proxy port
    #[arg(long, global = true)]
    port: Option<u16>,

    /// Extra CSP domains
    #[arg(long = "csp", global = true)]
    csp_domains: Vec<String>,

    /// CSP mode: "extend" (add to upstream CSP) or "override" (replace upstream CSP)
    #[arg(long = "csp-mode", global = true)]
    csp_mode: Option<String>,

    /// Run as relay server instead of client proxy
    #[arg(long, global = true)]
    relay: bool,

    /// Relay server base domain (for relay mode)
    #[arg(long, global = true)]
    relay_domain: Option<String>,

    /// Auth provider URL for token validation (relay mode)
    #[arg(long, env = "MCPR_AUTH_PROVIDER", global = true)]
    auth_provider: Option<String>,

    /// Shared secret between relay and auth provider
    #[arg(long, env = "MCPR_AUTH_PROVIDER_SECRET", global = true)]
    auth_provider_secret: Option<String>,

    /// Relay server URL (for gateway tunnel mode)
    #[arg(long, env = "MCPR_RELAY_URL", global = true)]
    relay_url: Option<String>,

    /// Don't start any tunnel (local-only mode)
    #[arg(long, global = true)]
    no_tunnel: bool,

    /// Emit structured JSON events to stdout
    #[arg(long, global = true)]
    events: bool,

    /// Cloud sync token (from cloud.mcpr.app project settings)
    #[arg(long, env = "MCPR_CLOUD_TOKEN", global = true)]
    cloud_token: Option<String>,

    /// Server slug for cloud routing (matches server name in cloud project)
    #[arg(long, global = true)]
    cloud_server: Option<String>,

    /// Force TUI on (default: auto-detect terminal)
    #[arg(long, global = true, env = "MCPR_TUI")]
    tui: bool,

    /// Force TUI off (for Docker/CI environments)
    #[arg(long, global = true, env = "MCPR_NO_TUI")]
    no_tui: bool,

    /// Graceful shutdown drain timeout in seconds
    #[arg(long, global = true, default_value = "30")]
    drain_timeout: u64,

    /// Log format for stderr output: json (default) or pretty
    #[arg(long, global = true, default_value = "json")]
    log_format: LogFormat,

    /// Admin API bind address (set to "none" to disable)
    #[arg(long, global = true, default_value = "127.0.0.1:9901")]
    admin_bind: String,
}

#[derive(Subcommand)]
enum Commands {
    /// Start proxy in foreground (default if no subcommand given)
    Run,
    /// Start proxy as a background daemon
    Start,
    /// Stop the running daemon
    Stop,
    /// Restart the daemon (stop + start)
    Restart,
    /// Show daemon status (PID, port, uptime)
    Status,
    /// Validate config file and exit
    Validate(ValidateArgs),
    /// Print version information and exit
    Version,
    /// Query proxy observability data (logs, slow calls, stats, sessions, clients)
    Proxy(ProxyArgs),
    /// Storage maintenance (stats, vacuum)
    Store(StoreArgs),
}

// ── Proxy query subcommands ────────────────────────────────────────────

#[derive(Parser)]
pub struct ProxyArgs {
    #[command(subcommand)]
    pub command: ProxyCommand,
}

/// Observability commands — read-only queries against the SQLite store.
/// These work without a running proxy (they open the DB file directly).
#[derive(Subcommand, Clone)]
pub enum ProxyCommand {
    /// Show recent request logs
    Logs(ProxyLogsArgs),
    /// Show slowest requests above a latency threshold
    Slow(ProxySlowArgs),
    /// Show per-tool aggregated metrics
    Stats(ProxyStatsArgs),
    /// List MCP sessions with client info
    Sessions(ProxySessionsArgs),
    /// Show client (AI model) breakdown
    Clients(ProxyClientsArgs),
}

/// Arguments for `mcpr proxy logs <name>`.
#[derive(Parser, Clone)]
pub struct ProxyLogsArgs {
    /// Proxy name to query (derived from upstream URL if not set in config)
    pub name: String,

    /// Number of recent rows to show
    #[arg(long, default_value = "50")]
    pub tail: i64,

    /// Time window: only rows newer than this duration (e.g., 1h, 30m, 7d)
    #[arg(long, default_value = "1h")]
    pub since: String,

    /// Filter to a specific tool name
    #[arg(long)]
    pub tool: Option<String>,

    /// Filter by status: ok, error, timeout
    #[arg(long)]
    pub status: Option<String>,

    /// Output as newline-delimited JSON (one object per line)
    #[arg(long)]
    pub json: bool,

    /// Poll for new rows every 500ms (like tail -f)
    #[arg(short, long)]
    pub follow: bool,
}

/// Arguments for `mcpr proxy slow <name>`.
#[derive(Parser, Clone)]
pub struct ProxySlowArgs {
    /// Proxy name to query
    pub name: String,

    /// Minimum latency to include (e.g., 500ms, 1s, 2s). Default: 500ms
    #[arg(long, default_value = "500ms")]
    pub threshold: String,

    /// Time window (e.g., 1h, 24h)
    #[arg(long, default_value = "1h")]
    pub since: String,

    /// Maximum rows to return
    #[arg(long, default_value = "20")]
    pub limit: i64,

    /// Output as newline-delimited JSON
    #[arg(long)]
    pub json: bool,
}

/// Arguments for `mcpr proxy stats <name>`.
#[derive(Parser, Clone)]
pub struct ProxyStatsArgs {
    /// Proxy name to query
    pub name: String,

    /// Aggregation window (e.g., 1h, 24h)
    #[arg(long, default_value = "1h")]
    pub since: String,

    /// Output as JSON snapshot (no table)
    #[arg(long)]
    pub json: bool,
}

/// Arguments for `mcpr proxy sessions <name>`.
#[derive(Parser, Clone)]
pub struct ProxySessionsArgs {
    /// Proxy name to query
    pub name: String,

    /// Only show active sessions (seen in last 5 minutes)
    #[arg(long)]
    pub active: bool,

    /// Filter by client name (e.g., claude-desktop)
    #[arg(long)]
    pub client: Option<String>,

    /// Time window for session start (e.g., 1h, 24h)
    #[arg(long, default_value = "1h")]
    pub since: String,

    /// Maximum rows to return
    #[arg(long, default_value = "50")]
    pub limit: i64,

    /// Output as newline-delimited JSON
    #[arg(long)]
    pub json: bool,
}

/// Arguments for `mcpr proxy clients <name>`.
#[derive(Parser, Clone)]
pub struct ProxyClientsArgs {
    /// Proxy name to query
    pub name: String,

    /// Lookback window (default longer: clients change slowly)
    #[arg(long, default_value = "7d")]
    pub since: String,

    /// Output as newline-delimited JSON
    #[arg(long)]
    pub json: bool,
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
pub struct RuntimeOptions {
    pub tui: bool,
    pub no_tui: bool,
    pub drain_timeout: u64,
    pub log_format: LogFormat,
    pub admin_bind: String,
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

    // -- Cloud sync --
    cloud: FileCloudConfig,

    // -- Logging --
    logging: FileLoggingConfig,

    // -- Runtime --
    no_tui: bool,
    drain_timeout: Option<u64>,
    log_format: Option<String>,

    // -- Admin --
    admin_bind: Option<String>,

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
#[allow(dead_code)]
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
    pub cloud_token: Option<String>,
    pub cloud_server: Option<String>,
    pub cloud_endpoint: Option<String>,
    pub cloud_batch_size: Option<usize>,
    pub cloud_flush_interval_ms: Option<u64>,
    pub runtime: RuntimeOptions,
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

/// Parse CLI args, load config file, and return the resolved action.
pub fn load() -> CliAction {
    let cli = Cli::parse();

    match cli.command {
        Some(Commands::Validate(args)) => return CliAction::Validate(args),
        Some(Commands::Version) => return CliAction::Version,
        Some(Commands::Proxy(args)) => return CliAction::Proxy(args.command),
        Some(Commands::Store(args)) => return CliAction::Store(args.command),
        Some(Commands::Stop) => return CliAction::Stop,
        Some(Commands::Status) => return CliAction::Status,
        Some(Commands::Run) | None | Some(Commands::Start) | Some(Commands::Restart) => {
            // Continue to load config and determine run mode.
            // Start/Restart need the same config resolution as Run.
        }
    }

    // Remember if this is a start/restart for after config resolution.
    let is_start = matches!(cli.command, Some(Commands::Start));
    let is_restart = matches!(cli.command, Some(Commands::Restart));

    let (file, config_path) = FileConfig::load();

    // Merge TUI/runtime settings: CLI flags override file config
    let runtime = RuntimeOptions {
        tui: cli.tui,
        no_tui: cli.no_tui || file.no_tui,
        drain_timeout: if cli.drain_timeout != 30 {
            cli.drain_timeout
        } else {
            file.drain_timeout.unwrap_or(30)
        },
        log_format: if cli.log_format != LogFormat::Json {
            cli.log_format
        } else {
            file.log_format
                .as_deref()
                .and_then(|s| s.parse().ok())
                .unwrap_or(LogFormat::Json)
        },
        admin_bind: if cli.admin_bind != "127.0.0.1:9901" {
            cli.admin_bind.clone()
        } else {
            file.admin_bind
                .clone()
                .unwrap_or_else(|| "127.0.0.1:9901".to_string())
        },
    };

    let mode = if cli.relay || file.is_relay() {
        load_relay(cli, file, runtime)
    } else {
        load_gateway(cli, file, config_path, runtime)
    };

    if is_start {
        CliAction::Start(mode)
    } else if is_restart {
        CliAction::Restart(mode)
    } else {
        CliAction::Run(mode)
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
                if config.relay_domain().is_none() {
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

            // Validate CSP mode
            if let Some(ref mode) = config.csp.mode
                && mode != "extend"
                && mode != "override"
            {
                issues.push((
                    "error",
                    format!("invalid csp.mode: {mode} (expected: extend, override)"),
                ));
            }

            // Validate log format
            if let Some(ref fmt) = config.log_format
                && fmt.parse::<LogFormat>().is_err()
            {
                issues.push((
                    "error",
                    format!("invalid log_format: {fmt} (expected: json, pretty)"),
                ));
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

fn load_relay(cli: Cli, file: FileConfig, _runtime: RuntimeOptions) -> Mode {
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

fn load_gateway(
    cli: Cli,
    file: FileConfig,
    config_path: Option<std::path::PathBuf>,
    runtime: RuntimeOptions,
) -> Mode {
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

            if std::io::IsTerminal::is_terminal(&std::io::stdin()) {
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
            } else {
                eprintln!(
                    "  {} Run interactively to save, or edit {} directly.",
                    colored::Colorize::dimmed("hint"),
                    path.display()
                );
            }
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

    Mode::Gateway(Box::new(GatewayConfig {
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
        cloud_token: cli.cloud_token.or(file.cloud.token),
        cloud_server: cli.cloud_server.or(file.cloud.server),
        cloud_endpoint: file.cloud.endpoint,
        cloud_batch_size: file.cloud.batch_size,
        cloud_flush_interval_ms: file.cloud.flush_interval_ms,
        runtime,
    }))
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

    // ── Cloud config parsing tests ─────────────────────────────────────

    #[test]
    fn cloud_config_parses_all_fields() {
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
    fn cloud_config_defaults_to_none() {
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
    fn cloud_config_partial_fields() {
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
    fn cloud_config_coexists_with_other_sections() {
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
    fn empty_cloud_section_uses_defaults() {
        let toml_str = r#"
            [cloud]
        "#;
        let config: FileConfig = toml::from_str(toml_str).unwrap();
        assert!(config.cloud.token.is_none());
        assert!(config.cloud.server.is_none());
    }
}
