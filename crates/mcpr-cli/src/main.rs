//! # mcpr (CLI binary)
//!
//! The main mcpr binary: an open-source reverse proxy for MCP applications.
//! This crate orchestrates all library crates into a runnable gateway or
//! relay server.
//!
//! ## Responsibilities
//!
//! - **Configuration** (`config`): CLI argument parsing (clap), TOML config
//!   file loading, and config validation. Resolves the run mode (gateway vs
//!   relay) and merges CLI args with file-based config.
//!
//! - **Proxy orchestration** (`proxy`): Catch-all HTTP handler that dispatches
//!   classified requests to the appropriate handler (MCP, widgets, passthrough).
//!
//! - **MCP request handling** (`mcp_handler`): Processes MCP JSON-RPC POST and
//!   SSE requests — forwards to upstream, rewrites responses, tracks sessions,
//!   and emits structured events.
//!
//! - **Widget serving** (`widgets`): Serves widget HTML pages, static assets,
//!   and widget list endpoints from local filesystem or upstream proxy.
//!
//! - **Passthrough** (`passthrough`): Forwards non-MCP requests (OAuth, .well-known,
//!   etc.) to upstream with URL rewriting.
//!
//! - **Terminal UI** (`tui`): Real-time dashboard showing connection status,
//!   request log, sessions, and cloud sync status. Built with ratatui.
//!
//! - **Logging** (`logger`): Multi-sink structured logging with support for
//!   stderr, TUI, and file sinks (daily/size rotation).
//!
//! - **Admin endpoints** (`admin`): Health check and readiness probe endpoints
//!   on a separate port for orchestration (k8s, docker, etc.).
//!
//! - **Onboarding** (`onboarding`): Interactive tunnel setup flow for first-time
//!   users connecting to tunnel.mcpr.app.
//!
//! ## Module Structure
//!
//! ```text
//! mcpr-cli/src/
//! +-- main.rs         # Entry point, gateway startup, signal handling
//! +-- config.rs       # CLI args, TOML config, validation
//! +-- proxy.rs        # Request dispatcher (classify -> handle)
//! +-- mcp_handler.rs  # MCP POST/SSE handling, session tracking, events
//! +-- widgets.rs      # Widget HTML serving, asset proxying
//! +-- passthrough.rs  # Non-MCP request forwarding
//! +-- admin.rs        # Health/readiness admin server
//! +-- onboarding.rs   # Interactive tunnel claim flow
//! +-- display.rs      # Startup banner and formatting
//! +-- logger/         # LogRouter, FileSink, StderrSink, TuiSink
//! +-- tui/            # Terminal UI (app loop, rendering, state)
//! ```

mod admin;
mod config;
mod display;
pub mod logger;
mod mcp_handler;
mod onboarding;
mod passthrough;
mod proxy;
mod tui;
mod widgets;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::RwLock;

use mcpr_integrations::EventEmitter;
use mcpr_proxy::forwarding::UpstreamClient;

use axum::Router;
use axum::extract::DefaultBodyLimit;
use tower_http::cors::{Any, CorsLayer};

use config::{CliAction, GatewayConfig, Mode};
use display::log_startup;
use logger::{
    DEFAULT_MAX_FILES, FileSink, FileSinkConfig, LogRouter, LogSink, Rotation, StderrSink, TuiSink,
    prefix_from_upstream,
};
use mcpr_protocol::session::MemorySessionStore;
use mcpr_proxy::RewriteConfig;
use proxy::proxy_routes;
use tui::SharedTuiState;
use widgets::WidgetSource;

/// Global drain flag — set to true when graceful shutdown begins.
static IS_DRAINING: AtomicBool = AtomicBool::new(false);

pub const DEFAULT_MAX_REQUEST_BODY_SIZE: usize = 5 * 1024 * 1024;
pub const DEFAULT_MAX_RESPONSE_BODY_SIZE: usize = 10 * 1024 * 1024;
pub const DEFAULT_MAX_CONCURRENT_UPSTREAM: usize = 100;
pub const DEFAULT_CONNECT_TIMEOUT_SECS: u64 = 5;
pub const DEFAULT_REQUEST_TIMEOUT_SECS: u64 = 30;

pub fn build_app(state: AppState) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any)
        .expose_headers(Any);

    let max_request = state.max_request_body;

    let app: Router<AppState> = Router::new();
    let app = proxy_routes(app);
    app.with_state(state)
        .layer(DefaultBodyLimit::max(max_request))
        .layer(cors)
}

#[derive(Clone)]
pub struct AppState {
    pub mcp_upstream: String,
    pub widget_source: Option<WidgetSource>,
    pub rewrite_config: Arc<RwLock<RewriteConfig>>,
    pub upstream: UpstreamClient,
    pub tui_state: SharedTuiState,
    pub logger: LogRouter,
    pub events: Arc<dyn EventEmitter>,
    pub sessions: MemorySessionStore,
    pub max_request_body: usize,
    pub max_response_body: usize,
}

/// Adapter to bridge mcpr-tunnel's TunnelStatusCallback to TUI state.
struct TuiTunnelStatus(SharedTuiState);

impl mcpr_tunnel::TunnelStatusCallback for TuiTunnelStatus {
    fn on_connected(&self, _url: &str) {
        self.0.lock().unwrap().tunnel_status = tui::ConnectionStatus::Connected;
    }
    fn on_disconnected(&self) {
        self.0.lock().unwrap().tunnel_status = tui::ConnectionStatus::Disconnected;
    }
    fn on_evicted(&self) {
        self.0.lock().unwrap().tunnel_status = tui::ConnectionStatus::Evicted;
    }
}

#[tokio::main]
async fn main() {
    match config::load() {
        CliAction::Run(mode) => match mode {
            Mode::Relay(cfg) => {
                mcpr_tunnel::relay::start_relay(cfg).await;
            }
            Mode::Gateway(cfg) => {
                run_gateway(*cfg).await;
            }
        },
        CliAction::Validate(args) => {
            let issues = config::validate_config(args.config.as_deref());
            let mut has_error = false;
            for (severity, msg) in &issues {
                match *severity {
                    "error" => {
                        has_error = true;
                        eprintln!("  {} {msg}", colored::Colorize::red("error"),);
                    }
                    "warn" => {
                        eprintln!("  {} {msg}", colored::Colorize::yellow("warn"),);
                    }
                    _ => {
                        eprintln!("  {} {msg}", colored::Colorize::green("ok"),);
                    }
                }
            }
            std::process::exit(if has_error { 1 } else { 0 });
        }
        CliAction::Version => {
            println!(
                "{}",
                serde_json::json!({
                    "version": env!("CARGO_PKG_VERSION"),
                    "target": option_env!("TARGET").unwrap_or("unknown"),
                })
            );
        }
    }
}

async fn run_gateway(cfg: GatewayConfig) {
    let tui_state = tui::new_shared_state();

    let mcp = cfg.mcp.expect("mcp is required in mcpr.toml or --mcp");

    // Validate MCP URL format
    validate_mcp_url(&mcp);

    let widget_source = cfg.widgets.as_ref().map(|w| {
        if w.starts_with("http://") || w.starts_with("https://") {
            WidgetSource::Proxy(w.clone())
        } else {
            WidgetSource::Static(w.clone())
        }
    });

    // Bind listener first — in tunnel mode with no explicit port, use port 0 (random)
    let bind_port = if !cfg.no_tunnel && cfg.port.is_none() {
        0
    } else {
        cfg.port.expect("port is required in mcpr.toml or --port")
    };
    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{bind_port}"))
        .await
        .expect("Failed to bind");
    let actual_port = listener.local_addr().unwrap().port();

    // Determine public URL
    let public_url = if cfg.no_tunnel {
        // No tunnel — mark as connected (local-only)
        tui_state.lock().unwrap().tunnel_status = tui::ConnectionStatus::Connected;
        format!("http://localhost:{actual_port}")
    } else {
        let relay_url = cfg.relay_url.as_deref().unwrap();
        let config_path = cfg.config_path.clone();

        // If using tunnel.mcpr.app without a token, run the interactive claim flow
        let is_mcpr_relay = relay_url.contains("tunnel.mcpr.app");
        let (token, desired_subdomain) = if is_mcpr_relay && cfg.tunnel_token.is_none() {
            eprintln!(
                "\n  {} Welcome! Let's set up your tunnel.\n",
                colored::Colorize::cyan("→"),
            );
            match onboarding::run_claim_flow(cfg.tunnel_subdomain.as_deref()).await {
                Ok(result) => {
                    let save_path = config_path
                        .clone()
                        .unwrap_or_else(|| std::env::current_dir().unwrap().join("mcpr.toml"));
                    // Create the file if it doesn't exist so save_tunnel_config can read+update it
                    if !save_path.exists() {
                        let _ = std::fs::write(&save_path, "");
                    }
                    GatewayConfig::save_tunnel_config(
                        &save_path,
                        &result.token,
                        &result.subdomain,
                        result.anonymous,
                    );
                    if result.anonymous {
                        eprintln!(
                            "\n  {} This is a temporary tunnel (1 week). To keep a custom subdomain, re-run with --no-tunnel and reconfigure.",
                            colored::Colorize::yellow("!"),
                        );
                    } else {
                        eprintln!(
                            "\n  {} We sent a verification link to your email.",
                            colored::Colorize::yellow("!"),
                        );
                        eprintln!(
                            "  {} Verify to keep '{}' permanently — it's reserved for 72 hours.\n",
                            colored::Colorize::yellow("!"),
                            result.subdomain,
                        );
                    }
                    let anonymous = result.anonymous;
                    let pair = (result.token, Some(result.subdomain));
                    if anonymous {
                        tui_state.lock().unwrap().tunnel_anonymous = true;
                    }
                    pair
                }
                Err(e) => {
                    eprintln!(
                        "{}: Onboarding failed: {}",
                        colored::Colorize::red("error"),
                        e
                    );
                    std::process::exit(1);
                }
            }
        } else {
            GatewayConfig::resolve_tunnel_identity(cfg.tunnel_subdomain, cfg.tunnel_token, || {
                let new_token = uuid::Uuid::new_v4().to_string();
                if let Some(path) = &config_path {
                    GatewayConfig::save_tunnel_token(path, &new_token);
                }
                new_token
            })
        };

        {
            let mut state = tui_state.lock().unwrap();
            state.tunnel_status = tui::ConnectionStatus::Connecting;
            if cfg.tunnel_anonymous {
                state.tunnel_anonymous = true;
            }
        }

        match mcpr_tunnel::start_tunnel_client(
            actual_port,
            relay_url,
            &token,
            desired_subdomain.as_deref(),
            TuiTunnelStatus(tui_state.clone()),
        )
        .await
        {
            Ok(url) => url,
            Err(e) => {
                eprintln!(
                    "{}: Failed to connect to relay: {}",
                    colored::Colorize::red("error"),
                    e
                );
                eprintln!("Use --no-tunnel for local-only mode");
                std::process::exit(1);
            }
        }
    };

    let proxy_domain = public_url
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/')
        .to_string();

    let rewrite_config = RewriteConfig {
        proxy_url: public_url.clone(),
        proxy_domain,
        mcp_upstream: mcp.clone(),
        extra_csp_domains: cfg.csp_domains.clone(),
        csp_mode: cfg.csp_mode,
    };

    let connect_timeout =
        Duration::from_secs(cfg.connect_timeout.unwrap_or(DEFAULT_CONNECT_TIMEOUT_SECS));
    let request_timeout =
        Duration::from_secs(cfg.request_timeout.unwrap_or(DEFAULT_REQUEST_TIMEOUT_SECS));

    // Determine TUI mode
    let use_tui = match (cfg.runtime.tui, cfg.runtime.no_tui) {
        (true, _) => true,
        (_, true) => false,
        _ => std::io::IsTerminal::is_terminal(&std::io::stdout()),
    };

    // Build log sinks
    let mut sinks: Vec<Box<dyn LogSink>> = Vec::new();
    if use_tui {
        sinks.push(Box::new(TuiSink::new(tui_state.clone())));
    } else {
        sinks.push(Box::new(StderrSink::new(cfg.runtime.log_format)));
    }

    if cfg.log_file {
        let rotation = match cfg.log_rotation.as_deref() {
            Some(s) if s.starts_with("size:") => {
                let size_str = s.trim_start_matches("size:");
                let bytes = parse_size(size_str).unwrap_or(50 * 1024 * 1024);
                Rotation::Size(bytes)
            }
            _ => Rotation::Daily,
        };
        let dir = cfg.log_dir.unwrap_or_else(|| "./logs".to_string());
        match FileSink::new(FileSinkConfig {
            dir: std::path::PathBuf::from(&dir),
            rotation,
            max_files: DEFAULT_MAX_FILES,
            prefix: prefix_from_upstream(&mcp),
        }) {
            Ok(sink) => {
                sinks.push(Box::new(sink));
            }
            Err(e) => {
                eprintln!(
                    "{}: failed to init file logger: {e}",
                    colored::Colorize::red("error"),
                );
            }
        }
    }

    let log_handle = LogRouter::start(sinks);

    let upstream = UpstreamClient {
        http_client: reqwest::Client::builder()
            .connect_timeout(connect_timeout)
            .pool_idle_timeout(Duration::from_secs(90))
            .pool_max_idle_per_host(10)
            .build()
            .expect("Failed to build HTTP client"),
        semaphore: Arc::new(tokio::sync::Semaphore::new(
            cfg.max_concurrent_upstream
                .unwrap_or(DEFAULT_MAX_CONCURRENT_UPSTREAM),
        )),
        request_timeout,
    };

    // Pick event emitter: cloud sync if token is set, otherwise noop.
    // (TUI owns stdout, so StdoutEmitter can't run here. CloudEmitter uses HTTP.)
    let events: Arc<dyn EventEmitter> = if let Some(ref token) = cfg.cloud_token {
        let endpoint = cfg
            .cloud_endpoint
            .clone()
            .unwrap_or_else(|| "https://api.mcpr.app".to_string());
        let cloud_endpoint = format!("{}/api/ingest-events", endpoint.trim_end_matches('/'));
        tui_state.lock().unwrap().cloud_endpoint = Some(cloud_endpoint.clone());
        let tui_for_cloud = tui_state.clone();
        Arc::new(mcpr_integrations::CloudEmitter::new(
            mcpr_integrations::CloudEmitterConfig {
                endpoint: cloud_endpoint,
                token: token.clone(),
                server: cfg.cloud_server.clone(),
                batch_size: cfg.cloud_batch_size.unwrap_or(100),
                flush_interval: Duration::from_millis(cfg.cloud_flush_interval_ms.unwrap_or(5000)),
                on_flush: Some(std::sync::Arc::new(move |status| {
                    if let Ok(mut state) = tui_for_cloud.lock() {
                        state.cloud_sync = Some(match status {
                            mcpr_integrations::SyncStatus::Ok { count } => {
                                tui::state::CloudSyncStatus::Ok { count }
                            }
                            mcpr_integrations::SyncStatus::Failed { message } => {
                                tui::state::CloudSyncStatus::Failed { message }
                            }
                        });
                    }
                })),
            },
        ))
    } else {
        Arc::new(mcpr_integrations::NoopEmitter)
    };

    let state = AppState {
        mcp_upstream: mcp.clone(),
        widget_source,
        rewrite_config: Arc::new(RwLock::new(rewrite_config)),
        upstream: upstream.clone(),
        tui_state: tui_state.clone(),
        logger: log_handle.router.clone(),
        events,
        sessions: MemorySessionStore::new(),
        max_request_body: cfg
            .max_request_body_size
            .unwrap_or(DEFAULT_MAX_REQUEST_BODY_SIZE),
        max_response_body: cfg
            .max_response_body_size
            .unwrap_or(DEFAULT_MAX_RESPONSE_BODY_SIZE),
    };

    // Initial connectivity probe — warn early if the MCP URL seems wrong
    probe_mcp_upstream(&mcp, &upstream.http_client, &tui_state).await;

    let health_state = state.clone();
    let tui_sessions = state.sessions.clone();

    let app = build_app(state);

    log_startup(
        &tui_state,
        actual_port,
        &public_url,
        &mcp,
        cfg.widgets.as_deref(),
    );

    let drain_timeout = cfg.runtime.drain_timeout;
    let admin_bind = cfg.runtime.admin_bind.clone();

    // Create a shutdown signal that responds to SIGTERM and SIGINT
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);

    // Spawn the axum server with graceful shutdown
    let shutdown_for_server = shutdown_tx.subscribe();
    tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let mut rx = shutdown_for_server;
                let _ = rx.changed().await;
            })
            .await
            .expect("Server failed");
    });

    // Spawn admin server (health + readiness endpoints)
    if admin_bind != "none" {
        let admin_tui_state = tui_state.clone();
        let admin_shutdown = shutdown_tx.subscribe();
        tokio::spawn(async move {
            admin::start_admin_server(&admin_bind, admin_tui_state, admin_shutdown).await;
        });
    }

    // Spawn health check task: periodically probe MCP + widgets status
    tokio::spawn(async move {
        health_check_loop(health_state).await;
    });

    // Spawn signal handler
    let shutdown_trigger = shutdown_tx.clone();
    tokio::spawn(async move {
        let ctrl_c = tokio::signal::ctrl_c();

        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            let mut sigterm = signal(SignalKind::terminate()).expect("Failed to register SIGTERM");
            tokio::select! {
                _ = ctrl_c => {},
                _ = sigterm.recv() => {},
            }
        }

        #[cfg(not(unix))]
        {
            ctrl_c.await.expect("Failed to listen for ctrl-c");
        }

        eprintln!("[mcpr] Received shutdown signal, draining...");
        IS_DRAINING.store(true, Ordering::SeqCst);
        let _ = shutdown_trigger.send(true);
    });

    if use_tui {
        // Run the TUI on a blocking thread (it reads stdin).
        let tui_shutdown = shutdown_tx.subscribe();
        let tui_handle = tokio::task::spawn_blocking(move || {
            tui::run(tui_state, tui_sessions, tui_shutdown).expect("TUI failed");
        });
        tui_handle.await.unwrap();
        // TUI exited (user pressed q or signal received) — trigger shutdown
        let _ = shutdown_tx.send(true);
    } else {
        if !cfg.runtime.no_tui {
            eprintln!("[mcpr] No terminal detected — TUI disabled. Running in headless mode.");
        }
        // Wait for shutdown signal
        let _ = shutdown_rx.changed().await;
    }

    // Graceful drain: wait for in-flight requests
    eprintln!("[mcpr] Waiting up to {drain_timeout}s for in-flight requests...");
    tokio::time::sleep(Duration::from_secs(drain_timeout.min(5))).await;

    // Gracefully flush log sinks
    log_handle.shutdown().await;
    eprintln!("[mcpr] Shutdown complete.");
}

/// Parse a human-readable size string like "50MB", "100KB", "1GB" into bytes.
fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim();
    let (num_str, multiplier) = if let Some(n) = s.strip_suffix("GB") {
        (n, 1024 * 1024 * 1024)
    } else if let Some(n) = s.strip_suffix("MB") {
        (n, 1024 * 1024)
    } else if let Some(n) = s.strip_suffix("KB") {
        (n, 1024)
    } else {
        (s, 1)
    };
    num_str.trim().parse::<u64>().ok().map(|n| n * multiplier)
}

/// Validate MCP URL format at startup. Exits with an error for clearly invalid URLs,
/// warns for suspicious patterns that might indicate a misconfiguration.
fn validate_mcp_url(url: &str) {
    // Must be parseable as a URL
    let parsed = match url::Url::parse(url) {
        Ok(u) => u,
        Err(e) => {
            eprintln!(
                "\n  {}: invalid MCP URL \"{}\": {}",
                colored::Colorize::red("error"),
                url,
                e,
            );
            eprintln!(
                "  {} Expected format: http://host:port or https://host/path\n",
                colored::Colorize::dimmed("hint"),
            );
            std::process::exit(1);
        }
    };

    // Must have http or https scheme
    match parsed.scheme() {
        "http" | "https" => {}
        scheme => {
            eprintln!(
                "\n  {}: unsupported scheme \"{}\" in MCP URL \"{}\"",
                colored::Colorize::red("error"),
                scheme,
                url,
            );
            eprintln!(
                "  {} MCP URLs must use http:// or https://\n",
                colored::Colorize::dimmed("hint"),
            );
            std::process::exit(1);
        }
    }

    // Must have a host
    if parsed.host_str().is_none() {
        eprintln!(
            "\n  {}: MCP URL \"{}\" has no host",
            colored::Colorize::red("error"),
            url,
        );
        eprintln!(
            "  {} Expected format: http://host:port or https://host/path\n",
            colored::Colorize::dimmed("hint"),
        );
        std::process::exit(1);
    }
}

/// Probe the MCP upstream at startup by sending an `initialize` JSON-RPC request.
/// This validates both connectivity and that the endpoint speaks MCP protocol.
async fn probe_mcp_upstream(url: &str, client: &reqwest::Client, tui_state: &SharedTuiState) {
    let (status, warning) = check_mcp_endpoint(url, client).await;
    let mut s = tui_state.lock().unwrap();
    s.mcp_status = status;
    s.mcp_warning = warning;
}

/// Send an MCP `initialize` request and classify the result.
/// Returns (status, optional warning message).
async fn check_mcp_endpoint(
    url: &str,
    client: &reqwest::Client,
) -> (tui::ConnectionStatus, Option<String>) {
    let init_body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 0,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-03-26",
            "capabilities": {},
            "clientInfo": {
                "name": "mcpr-probe",
                "version": "0.1.0"
            }
        }
    });

    let resp = match client
        .post(url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .json(&init_body)
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            let hint = if e.is_connect() {
                "Cannot connect. Is the MCP server running?"
            } else if e.is_timeout() {
                "Connection timed out. Check host and port."
            } else {
                "Cannot reach server. Check the URL."
            };
            return (tui::ConnectionStatus::Disconnected, Some(hint.to_string()));
        }
    };

    let status_code = resp.status().as_u16();

    // Auth-protected servers return 401/403 — the server is reachable and likely MCP,
    // we just can't verify with a probe. Mark as connected; the first real client
    // initialize will confirm status.
    if status_code == 401 || status_code == 403 {
        return (
            tui::ConnectionStatus::Connected,
            Some(
                "Server requires authentication. Status will update on first client connection."
                    .to_string(),
            ),
        );
    }

    // Read the body (capped to avoid OOM on non-MCP endpoints)
    let body_bytes = match resp.bytes().await {
        Ok(b) => b,
        Err(_) => {
            return (
                tui::ConnectionStatus::Connected,
                Some("Server reachable but response unreadable".to_string()),
            );
        }
    };

    // Try to parse as JSON-RPC response (possibly SSE-wrapped)
    let body_text = String::from_utf8_lossy(&body_bytes);

    // Handle SSE-wrapped response: extract JSON from "data: {...}" lines.
    // SSE format may include "event: message\n" before the data line.
    let json_str = body_text
        .lines()
        .find_map(|line| {
            let data = line.strip_prefix("data:")?.trim();
            if data.is_empty() {
                None
            } else {
                Some(data.to_string())
            }
        })
        .unwrap_or_else(|| body_text.to_string());

    let parsed: serde_json::Value = match serde_json::from_str(&json_str) {
        Ok(v) => v,
        Err(_) => {
            // Server responded but not with JSON — probably not an MCP server
            let hint = if status_code == 404 {
                "Server returned 404. Check the MCP endpoint path."
            } else if (300..400).contains(&status_code) {
                "Server returned a redirect. Check the URL."
            } else if body_text.trim_start().starts_with('<') {
                "Server returned HTML, not JSON-RPC. Not an MCP endpoint."
            } else {
                "Did not return JSON-RPC. Not an MCP endpoint?"
            };
            return (tui::ConnectionStatus::NotMcp, Some(hint.to_string()));
        }
    };

    // Check if it's a JSON-RPC 2.0 response
    if parsed.get("jsonrpc").and_then(|v| v.as_str()) != Some("2.0") {
        return (
            tui::ConnectionStatus::NotMcp,
            Some("Response is JSON but not JSON-RPC 2.0.".to_string()),
        );
    }

    // Check for error response
    if let Some(err) = parsed.get("error") {
        let code = err.get("code").and_then(|c| c.as_i64()).unwrap_or(0);
        let msg = err
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("unknown error");
        // Method not found means it's JSON-RPC but doesn't support MCP
        if code == -32601 {
            return (
                tui::ConnectionStatus::NotMcp,
                Some("JSON-RPC server but 'initialize' method not found.".to_string()),
            );
        }
        return (
            tui::ConnectionStatus::Connected,
            Some(format!("MCP init error: {msg}")),
        );
    }

    // Check for valid initialize result with serverInfo
    if let Some(result) = parsed.get("result") {
        if result.get("serverInfo").is_some() || result.get("capabilities").is_some() {
            // Valid MCP server
            return (tui::ConnectionStatus::Connected, None);
        }
        // Has a result but no serverInfo — might be MCP-ish but unexpected
        return (
            tui::ConnectionStatus::Connected,
            Some("Server responded but missing serverInfo in initialize result.".to_string()),
        );
    }

    // Fallback — got JSON-RPC but couldn't classify
    (tui::ConnectionStatus::Connected, None)
}

/// Periodically check MCP upstream and widget source connectivity.
async fn health_check_loop(app_state: AppState) {
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .unwrap();

    loop {
        // Check MCP upstream with protocol-level validation
        let (mcp_status, mcp_warning) = check_mcp_endpoint(&app_state.mcp_upstream, &http).await;

        // Discover widgets (reuses shared logic from widgets.rs)
        let names = widgets::discover_widget_names(&app_state).await;
        let widgets_status = if app_state.widget_source.is_none() {
            tui::ConnectionStatus::Unknown
        } else if names.is_empty() {
            tui::ConnectionStatus::Disconnected
        } else {
            tui::ConnectionStatus::Connected
        };
        let widget_count = if names.is_empty() {
            None
        } else {
            Some(names.len())
        };

        {
            let mut s = app_state.tui_state.lock().unwrap();
            s.mcp_status = mcp_status;
            s.mcp_warning = mcp_warning;
            s.widgets_status = widgets_status;
            s.widget_count = widget_count;
            s.widget_names = names;
        }

        // Emit heartbeat event to cloud (carries current proxy status)
        {
            let s = app_state.tui_state.lock().unwrap();
            let heartbeat_meta = serde_json::json!({
                "mcp_status": s.mcp_status.label(),
                "tunnel_status": s.tunnel_status.label(),
                "widgets_status": s.widgets_status.label(),
                "uptime_secs": s.started_at.elapsed().as_secs(),
                "request_count": s.request_count,
            });
            drop(s);

            app_state.events.emit(
                mcpr_integrations::McprEvent::new(mcpr_integrations::EventType::Heartbeat)
                    .status(mcpr_integrations::EventStatus::Ok)
                    .meta(heartbeat_meta),
            );
        }

        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
    }
}
