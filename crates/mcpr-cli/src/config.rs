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
    /// Start the proxy (daemon by default, foreground with --foreground).
    Start {
        mode: Mode,
        foreground: bool,
    },
    /// Stop the running daemon via SIGTERM.
    Stop,
    /// Stop + start the daemon.
    Restart(Mode),
    /// Show daemon status (PID, port, uptime, proxy name).
    Status,
    Validate(ValidateArgs),
    Version,
    /// Update mcpr to the latest version.
    Update,
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

    /// Path to config file (default: ./mcpr.toml)
    #[arg(long, short, global = true)]
    config: Option<String>,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the proxy (daemon by default, --foreground for attached mode)
    Start(StartArgs),
    /// Stop the running daemon
    Stop,
    /// Restart the daemon (stop + start)
    Restart,
    /// Show daemon status (PID, port, uptime, proxy name)
    Status,
    /// Validate config file and exit
    Validate(ValidateArgs),
    /// Print version information and exit
    Version,
    /// Update mcpr to the latest version
    Update,
    /// Query proxy observability data (logs, slow calls, stats, sessions, clients)
    Proxy(ProxyArgs),
    /// Storage maintenance (stats, vacuum)
    Store(StoreArgs),
}

/// Arguments for `mcpr start`.
#[derive(Parser, Clone)]
pub struct StartArgs {
    /// Run in foreground instead of daemonizing (for Docker, systemd, debugging)
    #[arg(long)]
    pub foreground: bool,
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
    /// Show proxy status overview (activity summary, active sessions, error rate)
    Status(ProxyStatusArgs),
    /// Drill into a single session — show session info and all its requests
    Session(ProxySessionArgs),
}

/// Arguments for `mcpr proxy logs [name]`.
#[derive(Parser, Clone)]
pub struct ProxyLogsArgs {
    /// Proxy name to query. Optional when only one proxy is running —
    /// auto-detected from the running daemon's config.
    pub name: Option<String>,

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

/// Arguments for `mcpr proxy slow [name]`.
#[derive(Parser, Clone)]
pub struct ProxySlowArgs {
    /// Proxy name to query (optional — auto-detected when one proxy is running)
    pub name: Option<String>,

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

/// Arguments for `mcpr proxy stats [name]`.
#[derive(Parser, Clone)]
pub struct ProxyStatsArgs {
    /// Proxy name to query (optional — auto-detected when one proxy is running)
    pub name: Option<String>,

    /// Aggregation window (e.g., 1h, 24h)
    #[arg(long, default_value = "1h")]
    pub since: String,

    /// Output as JSON snapshot (no table)
    #[arg(long)]
    pub json: bool,
}

/// Arguments for `mcpr proxy sessions [name]`.
#[derive(Parser, Clone)]
pub struct ProxySessionsArgs {
    /// Proxy name to query (optional — auto-detected when one proxy is running)
    pub name: Option<String>,

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

/// Arguments for `mcpr proxy clients [name]`.
#[derive(Parser, Clone)]
pub struct ProxyClientsArgs {
    /// Proxy name to query (optional — auto-detected when one proxy is running)
    pub name: Option<String>,

    /// Lookback window (default longer: clients change slowly)
    #[arg(long, default_value = "7d")]
    pub since: String,

    /// Output as newline-delimited JSON
    #[arg(long)]
    pub json: bool,
}

/// Arguments for `mcpr proxy status [name]`.
#[derive(Parser, Clone)]
pub struct ProxyStatusArgs {
    /// Proxy name to query (optional — auto-detected when one proxy is running)
    pub name: Option<String>,

    /// Lookback window for activity summary (e.g., 1h, 24h)
    #[arg(long, default_value = "1h")]
    pub since: String,

    /// Output as JSON snapshot
    #[arg(long)]
    pub json: bool,
}

/// Arguments for `mcpr proxy session <session_id>`.
#[derive(Parser, Clone)]
pub struct ProxySessionArgs {
    /// Session ID to look up
    pub session_id: String,

    /// Output as JSON
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
    /// Enable tunnel for a public URL. Default: false (proxy-only mode).
    enabled: bool,
    relay_url: Option<String>,
    token: Option<String>,
    subdomain: Option<String>,
    anonymous: bool,
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
    // -- Shared --
    port: Option<u16>,
    mode: Option<String>, // "relay" | "gateway" (default)

    // -- Gateway --
    mcp: Option<String>,
    widgets: Option<String>,
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
    /// Whether tunnel is enabled. Default: false (proxy-only mode).
    pub tunnel: bool,
    pub config_path: Option<std::path::PathBuf>,
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

        if contents.contains("token =") || contents.contains("token=") {
            return; // already set
        }

        let new_contents = if contents.contains("[tunnel]") {
            contents.replacen("[tunnel]", &format!("[tunnel]\ntoken = \"{token}\""), 1)
        } else {
            format!("{}\n\n[tunnel]\ntoken = \"{token}\"\n", contents.trim_end())
        };

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
        Some(Commands::Update) => return CliAction::Update,
        Some(Commands::Proxy(args)) => return CliAction::Proxy(args.command),
        Some(Commands::Store(args)) => return CliAction::Store(args.command),
        Some(Commands::Stop) => return CliAction::Stop,
        Some(Commands::Status) => return CliAction::Status,
        Some(Commands::Start(_)) | None | Some(Commands::Restart) => {
            // Continue to load config and determine run mode.
        }
    }

    let foreground = matches!(&cli.command, Some(Commands::Start(args)) if args.foreground);
    let is_restart = matches!(cli.command, Some(Commands::Restart));

    let (file, config_path) = FileConfig::load(cli.config.as_deref());

    let runtime = RuntimeOptions {
        drain_timeout: file.drain_timeout.unwrap_or(30),
        log_format: file
            .log_format
            .as_deref()
            .and_then(|s| s.parse().ok())
            .unwrap_or(LogFormat::Json),
        admin_bind: file
            .admin_bind
            .clone()
            .unwrap_or_else(|| "127.0.0.1:9901".to_string()),
    };

    let mode = if file.is_relay() {
        load_relay(file, runtime)
    } else {
        load_gateway(file, config_path, runtime)
    };

    if is_restart {
        CliAction::Restart(mode)
    } else {
        CliAction::Start { mode, foreground }
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

fn load_gateway(
    file: FileConfig,
    config_path: Option<std::path::PathBuf>,
    runtime: RuntimeOptions,
) -> Mode {
    let csp_mode = match file.csp.mode.as_deref() {
        Some(m) => parse_csp_mode(m),
        None => CspMode::default(),
    };

    Mode::Gateway(Box::new(GatewayConfig {
        mcp: file.mcp,
        widgets: file.widgets,
        port: file.port,
        csp_domains: file.csp.domains,
        csp_mode,
        relay_url: Some(
            file.tunnel
                .relay_url
                .unwrap_or_else(|| "https://tunnel.mcpr.app".to_string()),
        ),
        tunnel_token: file.tunnel.token,
        tunnel_subdomain: file.tunnel.subdomain,
        tunnel_anonymous: file.tunnel.anonymous,
        tunnel: file.tunnel.enabled,
        config_path,
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
