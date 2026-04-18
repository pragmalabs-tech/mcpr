//! # mcpr (CLI binary)
//!
//! Multi-process architecture:
//! - **mcprd** (daemon/supervisor): `mcpr start` — no config needed, just manages
//!   proxy and relay lifecycles and monitors health.
//! - **proxy** (standalone): `mcpr proxy run <config>` — snapshots config, forks to
//!   background, runs the MCP gateway. Self-terminates if the daemon dies.
//! - **relay** (singleton): `mcpr relay start <config>` — tunnel relay server that
//!   accepts WebSocket connections and assigns subdomains. One per machine.
//!
//! All state lives under `~/.mcpr/`.
//!
//! ## Module Structure
//!
//! Three-layer architecture: **render** (terminal output), **logic** (data operations),
//! **cmd** (thin command dispatch).
//!
//! ```text
//! mcpr-cli/src/
//! +-- main.rs           # Entry point, daemon bootstrap, gateway runtime
//! +-- config.rs         # CLI args (clap), TOML config, subcommands
//! +-- render.rs         # All terminal output — tables, colors, formatting
//! +-- logic/            # Core business logic (no printing)
//! |   +-- daemon.rs     #   Daemon status queries
//! |   +-- proxy.rs      #   Proxy lifecycle (start/stop/restart)
//! |   +-- relay.rs      #   Relay lifecycle (stop/restart/status)
//! |   +-- query.rs      #   DB engine, time parsing, threshold parsing
//! +-- cmd/              # Thin command handlers (logic → render)
//! |   +-- proxy.rs      #   Proxy lifecycle commands
//! |   +-- relay.rs      #   Relay lifecycle commands
//! |   +-- observe.rs    #   Observability commands (logs, stats, sessions, …)
//! |   +-- store.rs      #   Store maintenance commands
//! |   +-- setup.rs      #   Interactive setup wizard (mcpr proxy setup)
//! +-- daemon.rs         # mcprd supervisor (fork, PID file, health monitor)
//! +-- proxy_lock.rs     # Per-proxy lockfiles under ~/.mcpr/proxies/
//! +-- relay_lock.rs     # Singleton relay lockfile under ~/.mcpr/relay/
//! +-- proxy.rs          # Request dispatcher (classify → handle)
//! +-- mcp_handler.rs    # MCP POST/SSE handling, session tracking, store events
//! +-- widgets.rs        # Widget HTML serving, asset proxying
//! +-- passthrough.rs    # Non-MCP request forwarding
//! +-- admin.rs          # Health/readiness admin server
//! ```
//!
//! The event bus (routes `ProxyEvent` to sinks) lives in
//! `mcpr_core::event`; sink implementations (stderr, sqlite, cloud) live
//! in `mcpr_integrations`. This binary just registers them via
//! `EventManager`.

mod admin;
mod cmd;
mod config;
#[cfg(unix)]
mod daemon;
mod logic;
mod mcp_handler;
mod passthrough;
mod pipeline;
mod proxy;
#[allow(dead_code)]
mod proxy_lock;
mod relay_lock;
mod render;
mod state;
mod widgets;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::RwLock;

use mcpr_core::proxy::forwarding::UpstreamClient;

use axum::Router;
use axum::extract::DefaultBodyLimit;
use tower_http::cors::{Any, CorsLayer};

use config::{CliAction, GatewayConfig, Mode};
use mcpr_core::protocol::schema_manager::{MemorySchemaStore, SchemaManager};
use mcpr_core::protocol::session::MemorySessionStore;
use mcpr_core::proxy::RewriteConfig;
use mcpr_core::proxy::health::{self as proxy_health, SharedProxyHealth};
use proxy::proxy_routes;
use state::{AppState, ProxyState};
use widgets::WidgetSource;

/// Global drain flag — set to true when graceful shutdown begins.
static IS_DRAINING: AtomicBool = AtomicBool::new(false);

pub const DEFAULT_MAX_REQUEST_BODY_SIZE: usize = 5 * 1024 * 1024;
pub const DEFAULT_MAX_RESPONSE_BODY_SIZE: usize = 10 * 1024 * 1024;
pub const DEFAULT_MAX_CONCURRENT_UPSTREAM: usize = 100;
pub const DEFAULT_CONNECT_TIMEOUT_SECS: u64 = 5;
pub const DEFAULT_REQUEST_TIMEOUT_SECS: u64 = 30;

pub fn build_app(app_state: AppState) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any)
        .expose_headers(Any);

    let max_request = app_state.proxy.max_request_body;

    let app: Router<AppState> = Router::new();
    let app = proxy_routes(app);
    app.with_state(app_state)
        .layer(DefaultBodyLimit::max(max_request))
        .layer(cors)
}

/// Adapter to bridge mcpr-tunnel's TunnelStatusCallback to proxy health.
struct TunnelStatusAdapter(SharedProxyHealth);

impl mcpr_tunnel::TunnelStatusCallback for TunnelStatusAdapter {
    fn on_connected(&self, _url: &str) {
        proxy_health::lock_health(&self.0).tunnel_status =
            proxy_health::ConnectionStatus::Connected;
    }
    fn on_disconnected(&self) {
        proxy_health::lock_health(&self.0).tunnel_status =
            proxy_health::ConnectionStatus::Disconnected;
    }
    fn on_evicted(&self) {
        proxy_health::lock_health(&self.0).tunnel_status = proxy_health::ConnectionStatus::Evicted;
    }
}

/// Entry point — handles daemonization BEFORE starting tokio.
///
/// Tokio's IO driver uses epoll/kqueue file descriptors that don't survive
/// fork(). So we must fork first, then start the async runtime in the child.
fn main() {
    let mut action = config::load();

    // Daemonize before tokio starts (if needed).
    // Tokio's IO driver uses kqueue/epoll fds that don't survive fork().
    let ready_fd: Option<i32> = match &action {
        CliAction::Start { foreground: true } => {
            // Foreground mode — stop any existing daemon first.
            #[cfg(unix)]
            daemon::stop_daemon_if_running();
            None
        }
        CliAction::Start { foreground: false } => {
            #[cfg(unix)]
            {
                if daemon::ensure_not_running() {
                    // Daemon already running — nothing to do.
                    std::process::exit(0);
                }
                let fd = daemon::daemonize(Duration::from_secs(10)).unwrap_or_else(|e| {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                });
                Some(fd)
            }
            #[cfg(not(unix))]
            {
                eprintln!("error: daemon mode is not supported on this platform");
                eprintln!("  Use `mcpr start --foreground` or a service manager.");
                std::process::exit(1);
            }
        }
        CliAction::Restart { .. } => {
            #[cfg(unix)]
            {
                // Collect names of currently running proxies so we can re-launch them
                // after the daemon restarts.
                let running_names: Vec<String> = proxy_lock::list_proxies()
                    .into_iter()
                    .filter(|(_, s)| matches!(s, proxy_lock::LockStatus::Held(_)))
                    .map(|(name, _)| name)
                    .collect();

                // Check if relay was running.
                let relay_was_running =
                    matches!(relay_lock::check_lock(), relay_lock::LockStatus::Held(_));

                // Stop all managed proxies.
                let stopped = proxy_lock::stop_all_proxies();
                if !stopped.is_empty() {
                    eprintln!(
                        "Stopped {} managed proxy(ies): {}",
                        stopped.len(),
                        stopped.join(", ")
                    );
                }

                // Stop relay if running.
                if relay_lock::stop_relay() {
                    eprintln!("Stopped relay.");
                }

                // Pass the names to the child so it can re-launch after startup.
                if let CliAction::Restart {
                    restart_proxies,
                    restart_relay,
                    ..
                } = &mut action
                {
                    *restart_proxies = running_names;
                    *restart_relay = relay_was_running;
                }

                daemon::stop_daemon_if_running();
                let fd = daemon::daemonize(Duration::from_secs(10)).unwrap_or_else(|e| {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                });
                Some(fd)
            }
            #[cfg(not(unix))]
            {
                eprintln!("error: daemon management not supported on this platform");
                std::process::exit(1);
            }
        }
        CliAction::ProxyRun {
            mode: Mode::Gateway(cfg),
            replace,
            config_content,
            config_path: _,
        } => {
            #[cfg(unix)]
            {
                // Check that the daemon is running.
                if !matches!(daemon::check_status(), daemon::DaemonStatus::Running(_)) {
                    eprintln!("error: daemon not running — run `mcpr start` first");
                    std::process::exit(1);
                }

                let proxy_name = &cfg.name;

                // Conflict detection.
                match proxy_lock::check_lock(proxy_name) {
                    proxy_lock::LockStatus::Free => {}
                    proxy_lock::LockStatus::Stale(_) => {
                        proxy_lock::remove_lock(proxy_name);
                    }
                    proxy_lock::LockStatus::Held(info) => {
                        if *replace {
                            eprintln!("Stopping old \"{}\" (pid {})...", proxy_name, info.pid);
                            proxy_lock::stop_proxy(proxy_name);
                        } else {
                            eprintln!(
                                "error: proxy \"{}\" is already running (pid {}).",
                                proxy_name, info.pid
                            );
                            eprintln!("  Use --replace to stop the old one and start this one.");
                            std::process::exit(1);
                        }
                    }
                }

                // Snapshot config.
                if let Err(e) = proxy_lock::snapshot_config(proxy_name, config_content) {
                    eprintln!("error: failed to snapshot config: {e}");
                    std::process::exit(1);
                }

                // Double-fork to background (reuse daemon pattern).
                let fd = daemon::daemonize_proxy(proxy_name, Duration::from_secs(10))
                    .unwrap_or_else(|e| {
                        eprintln!("error: {e}");
                        std::process::exit(1);
                    });
                Some(fd)
            }
            #[cfg(not(unix))]
            {
                eprintln!("error: background proxy mode not supported on this platform");
                std::process::exit(1);
            }
        }
        CliAction::RelayRun {
            foreground: false,
            config_content,
            ..
        } => {
            #[cfg(unix)]
            {
                // Check that the daemon is running.
                if !matches!(daemon::check_status(), daemon::DaemonStatus::Running(_)) {
                    eprintln!("error: daemon not running — run `mcpr start` first");
                    std::process::exit(1);
                }

                // Check for existing relay.
                match relay_lock::check_lock() {
                    relay_lock::LockStatus::Free => {}
                    relay_lock::LockStatus::Stale(_) => relay_lock::remove_lock(),
                    relay_lock::LockStatus::Held(info) => {
                        eprintln!("error: relay already running (pid {})", info.pid);
                        std::process::exit(1);
                    }
                }

                // Snapshot config.
                if let Err(e) = relay_lock::snapshot_config(config_content) {
                    eprintln!("error: failed to snapshot config: {e}");
                    std::process::exit(1);
                }

                // Double-fork to background.
                let fd = daemon::daemonize_relay(Duration::from_secs(10)).unwrap_or_else(|e| {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                });
                Some(fd)
            }
            #[cfg(not(unix))]
            {
                eprintln!("error: background relay mode not supported on this platform");
                std::process::exit(1);
            }
        }
        CliAction::RelayRun {
            foreground: true, ..
        } => None,
        _ => None,
    };

    // Now start the tokio runtime (in the daemon child or the original process).
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("Failed to create tokio runtime")
        .block_on(async_main(action, ready_fd));
}

async fn async_main(action: CliAction, ready_fd: Option<i32>) {
    match action {
        CliAction::Start { .. } => {
            // Daemon supervisor — no config, no proxy. Already forked in main().
            #[cfg(unix)]
            daemon::run_supervisor(ready_fd).await;
            #[cfg(not(unix))]
            {
                eprintln!("error: daemon mode not supported on this platform");
                std::process::exit(1);
            }
        }
        CliAction::Stop => {
            // Stop all running proxies before stopping the daemon.
            let stopped = proxy_lock::stop_all_proxies();
            for name in &stopped {
                eprintln!("Stopped proxy \"{}\".", name);
            }
            #[cfg(unix)]
            daemon::stop_daemon();
            #[cfg(not(unix))]
            {
                eprintln!("error: daemon management not supported on this platform");
                std::process::exit(1);
            }
        }
        CliAction::Restart {
            restart_proxies,
            restart_relay,
        } => {
            // Spawn a background task to re-launch previously running proxies
            // and relay after the supervisor has had time to start up.
            if !restart_proxies.is_empty() || restart_relay {
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    for name in &restart_proxies {
                        match logic::proxy::start_proxy(name) {
                            Ok(_) => eprintln!("Restarted proxy \"{}\".", name),
                            Err(e) => eprintln!("Failed to restart proxy \"{}\": {}", name, e),
                        }
                    }
                    if restart_relay {
                        match logic::relay::start_relay_from_snapshot() {
                            Ok(_) => eprintln!("Restarted relay."),
                            Err(e) => eprintln!("Failed to restart relay: {}", e),
                        }
                    }
                });
            }
            // Daemonize already happened in main() before tokio started.
            #[cfg(unix)]
            daemon::run_supervisor(ready_fd).await;
            #[cfg(not(unix))]
            {
                eprintln!("error: daemon mode not supported on this platform");
                std::process::exit(1);
            }
        }
        CliAction::Status => {
            #[cfg(unix)]
            {
                let status = logic::daemon::get_status();
                let exit_code = render::daemon_status(status);
                if exit_code != 0 {
                    std::process::exit(exit_code);
                }
            }
            #[cfg(not(unix))]
            {
                eprintln!("error: daemon management not supported on this platform");
                std::process::exit(1);
            }
        }
        CliAction::Validate(args) => {
            let issues = config::validate_config(args.config.as_deref());
            let has_error = issues.iter().any(|(s, _)| *s == "error");
            render::validate_issues(&issues);
            std::process::exit(if has_error { 1 } else { 0 });
        }
        CliAction::Version => {
            render::version_info();
        }
        CliAction::Update => {
            eprintln!("Updating mcpr to the latest version...");
            let status = std::process::Command::new("sh")
                .args(["-c", "curl -fsSL https://mcpr.app/install.sh | sh"])
                .status();
            match status {
                Ok(s) if s.success() => {
                    // Auto-restart daemon if it's running, using the new binary.
                    #[cfg(unix)]
                    if matches!(daemon::check_status(), daemon::DaemonStatus::Running(_)) {
                        eprintln!("Restarting daemon with updated binary...");
                        let exe = std::env::current_exe().unwrap_or_else(|_| "mcpr".into());
                        let _ = std::process::Command::new(exe).arg("restart").status();
                    }
                }
                Ok(s) => std::process::exit(s.code().unwrap_or(1)),
                Err(e) => {
                    eprintln!("update failed: {e}");
                    std::process::exit(1);
                }
            }
        }
        CliAction::ProxySetup { cloud_url, output } => {
            if let Err(e) = cmd::setup::run_setup(&cloud_url, output.as_deref()).await {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
        CliAction::Proxy(cmd) => {
            cmd::handle_proxy_command(cmd);
        }
        CliAction::ProxyRun {
            mode, config_path, ..
        } => match mode {
            Mode::Relay(_) => {
                eprintln!("error: use `mcpr relay run` instead of `mcpr proxy run` for relay mode");
                std::process::exit(1);
            }
            Mode::Gateway(cfg) => {
                // Already forked in main(). Run gateway with proxy lockfile semantics.
                run_gateway_inner(*cfg, ready_fd, config_path).await;
            }
        },
        CliAction::Store(cmd) => {
            cmd::handle_store_command(cmd);
        }
        CliAction::RelayRun {
            relay_config,
            config_path,
            ..
        } => {
            run_relay_inner(relay_config, ready_fd, config_path).await;
        }
        CliAction::Relay(cmd) => {
            cmd::handle_relay_command(cmd);
        }
    }
}

/// Run a proxy gateway process. Called from `mcpr proxy run` only.
/// The proxy always writes a lockfile and monitors the daemon's health.
#[allow(unused_variables)]
async fn run_gateway_inner(cfg: GatewayConfig, ready_fd: Option<i32>, config_path: String) {
    let proxy_health_ref = proxy_health::new_shared_health();

    let mcp = match cfg.mcp {
        Some(url) => url,
        None => {
            eprintln!(
                "{}: `mcp` is required in mcpr.toml. Set it to your upstream MCP server URL, e.g. mcp = \"http://localhost:8080\"",
                colored::Colorize::red("error"),
            );
            std::process::exit(1);
        }
    };

    // Validate MCP URL format
    validate_mcp_url(&mcp);

    let proxy_name = cfg.name.clone();
    let proxy_name_for_shutdown = proxy_name.clone();

    let widget_source = cfg.widgets.as_ref().map(|w| {
        if w.starts_with("http://") || w.starts_with("https://") {
            WidgetSource::Proxy(w.clone())
        } else {
            WidgetSource::Static(w.clone())
        }
    });

    // Bind listener — in tunnel mode with no explicit port, use port 0 (OS picks random).
    // In proxy-only mode (no tunnel), default to 3000 if not specified.
    // Default port: 3000 for proxy-only mode, 0 (OS picks) for tunnel mode.
    let bind_port = cfg.port.unwrap_or(if cfg.tunnel { 0 } else { 3000 });
    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{bind_port}"))
        .await
        .expect("Failed to bind");
    let actual_port = listener.local_addr().unwrap().port();

    // Read daemon PID for the lockfile.
    #[cfg(unix)]
    let daemon_pid = daemon::read_pid_file().map(|i| i.pid);
    #[cfg(not(unix))]
    let daemon_pid: Option<u32> = None;

    // Write lockfile so status commands can find this process.
    #[cfg(unix)]
    {
        if let Err(e) = proxy_lock::write_lock(&proxy_name, actual_port, &config_path, daemon_pid) {
            eprintln!("error: failed to write lockfile: {e}");
            std::process::exit(1);
        }
        // Signal readiness to the parent process.
        if let Some(fd) = ready_fd {
            daemon::signal_ready(fd);
        }
    }

    // Determine public URL
    let public_url = if !cfg.tunnel {
        // No tunnel — mark as connected (local-only)
        proxy_health::lock_health(&proxy_health_ref).tunnel_status =
            proxy_health::ConnectionStatus::Connected;
        format!("http://localhost:{actual_port}")
    } else {
        let relay_url = cfg.relay_url.as_deref().unwrap();

        // Tunnel requires a token from mcpr.app.
        if cfg.tunnel_token.is_none() {
            eprintln!(
                "{}: No tunnel token configured. Register at https://mcpr.app to get one, then set `tunnel.token` in mcpr.toml.",
                colored::Colorize::red("error"),
            );
            std::process::exit(1);
        }

        let (token, desired_subdomain) =
            GatewayConfig::resolve_tunnel_identity(cfg.tunnel_subdomain, cfg.tunnel_token);

        proxy_health::lock_health(&proxy_health_ref).tunnel_status =
            proxy_health::ConnectionStatus::Connecting;

        match mcpr_tunnel::start_tunnel_client(
            actual_port,
            relay_url,
            &token,
            desired_subdomain.as_deref(),
            TunnelStatusAdapter(proxy_health_ref.clone()),
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
                eprintln!("Set `tunnel.enabled = false` in mcpr.toml to use proxy-only mode");
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
        csp: cfg.csp.clone(),
    };

    let connect_timeout =
        Duration::from_secs(cfg.connect_timeout.unwrap_or(DEFAULT_CONNECT_TIMEOUT_SECS));
    let request_timeout =
        Duration::from_secs(cfg.request_timeout.unwrap_or(DEFAULT_REQUEST_TIMEOUT_SECS));

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

    // Build event sinks — one pipeline, multiple destinations.
    let mut event_manager = mcpr_core::event::EventManager::new();

    // 1. Stderr sink — real-time console output.
    event_manager.register(Box::new(mcpr_integrations::StderrSink::new(
        cfg.runtime.log_format,
    )));

    // 2. SQLite sink — local storage for CLI queries.
    if let Some(db_path) = mcpr_integrations::store::path::resolve_db_path(None) {
        match mcpr_integrations::store::Store::open(mcpr_integrations::store::StoreConfig {
            db_path: db_path.clone(),
            mcpr_version: env!("CARGO_PKG_VERSION").to_string(),
        }) {
            Ok(store) => {
                eprintln!(
                    "  {} storage: {}",
                    colored::Colorize::dimmed("store"),
                    db_path.display()
                );
                event_manager.register(Box::new(mcpr_integrations::SqliteSink::new(store)));
            }
            Err(e) => {
                eprintln!(
                    "  {}: failed to open store: {e}",
                    colored::Colorize::yellow("warn"),
                );
            }
        }
    }

    // 3. Cloud sink — dashboard at cloud.mcpr.app.
    if let Some(ref token) = cfg.cloud_token {
        let endpoint = cfg
            .cloud_endpoint
            .clone()
            .unwrap_or_else(|| "https://api.mcpr.app".to_string());
        let cloud_endpoint = format!("{}/api/ingest-events", endpoint.trim_end_matches('/'));
        proxy_health::lock_health(&proxy_health_ref).cloud_endpoint = Some(cloud_endpoint.clone());
        let cloud_health = proxy_health_ref.clone();
        event_manager.register(Box::new(mcpr_integrations::CloudSink::new(
            mcpr_integrations::CloudSinkConfig {
                endpoint: cloud_endpoint,
                token: token.clone(),
                server: cfg.cloud_server.clone(),
                batch_size: cfg.cloud_batch_size.unwrap_or(100),
                flush_interval: Duration::from_millis(cfg.cloud_flush_interval_ms.unwrap_or(5000)),
                on_flush: Some(std::sync::Arc::new(move |status| {
                    use mcpr_integrations::sinks::cloud_sink::SyncStatus;
                    let mut h = proxy_health::lock_health(&cloud_health);
                    h.cloud_sync = Some(match status {
                        SyncStatus::Ok { count } => proxy_health::CloudSyncStatus::Ok { count },
                        SyncStatus::Failed { message } => {
                            proxy_health::CloudSyncStatus::Failed { message }
                        }
                    });
                })),
            },
        )));
    }

    let event_bus_handle = event_manager.start();

    let proxy = Arc::new(ProxyState {
        name: proxy_name.clone(),
        mcp_upstream: mcp.clone(),
        upstream: upstream.clone(),
        max_request_body: cfg
            .max_request_body_size
            .unwrap_or(DEFAULT_MAX_REQUEST_BODY_SIZE),
        max_response_body: cfg
            .max_response_body_size
            .unwrap_or(DEFAULT_MAX_RESPONSE_BODY_SIZE),
        rewrite_config: Arc::new(RwLock::new(rewrite_config)),
        widget_source,
        sessions: MemorySessionStore::new(),
        schema_manager: Arc::new(SchemaManager::new(
            proxy_name.clone(),
            MemorySchemaStore::new(),
        )),
        health: proxy_health_ref.clone(),
        event_bus: event_bus_handle.bus.clone(),
    });
    let app_state = AppState {
        proxy: proxy.clone(),
    };

    // Initial connectivity probe — warn early if the MCP URL seems wrong
    probe_mcp_upstream(&mcp, &upstream.http_client, &proxy_health_ref).await;

    let health_proxy = proxy.clone();

    let app = build_app(app_state);

    render::log_startup(
        &proxy_health_ref,
        actual_port,
        &public_url,
        &mcp,
        cfg.widgets.as_deref(),
        cfg.cloud_server.as_deref(),
    );

    // Persist the public URL so `mcpr proxy status` can display it.
    #[cfg(unix)]
    {
        let _ = proxy_lock::write_tunnel_url(&proxy_name_for_shutdown, &public_url);
        let _ = proxy_lock::write_upstream_url(&proxy_name_for_shutdown, &mcp);
    }

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
        let admin_health_ref = proxy_health_ref.clone();
        let admin_shutdown = shutdown_tx.subscribe();
        tokio::spawn(async move {
            admin::start_admin_server(&admin_bind, admin_health_ref, admin_shutdown).await;
        });
    }

    // Spawn health check task: periodically probe MCP + widgets status
    tokio::spawn(async move {
        health_check_loop(health_proxy).await;
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

    // Daemon watchdog: if the daemon dies, shut down this proxy.
    if let Some(dpid) = daemon_pid {
        let watchdog_shutdown = shutdown_tx.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(5)).await;
                #[cfg(unix)]
                if !daemon::is_process_alive(dpid) {
                    eprintln!("[mcpr] daemon died, shutting down...");
                    IS_DRAINING.store(true, Ordering::SeqCst);
                    let _ = watchdog_shutdown.send(true);
                    break;
                }
            }
        });
    }

    // Wait for shutdown signal (SIGTERM/SIGINT).
    // TUI is not started here — it will be available via `mcpr proxy view`.
    let _ = shutdown_rx.changed().await;

    // Graceful drain: wait for in-flight requests
    eprintln!("[mcpr] Waiting up to {drain_timeout}s for in-flight requests...");
    tokio::time::sleep(Duration::from_secs(drain_timeout.min(5))).await;

    // Flush all event sinks (stderr, sqlite, cloud).
    event_bus_handle.shutdown().await;

    // Clean up lockfile.
    proxy_lock::remove_lock(&proxy_name_for_shutdown);

    eprintln!("[mcpr] Shutdown complete.");
}

/// Run the relay server process. Called from `mcpr relay run` / `mcpr relay start`.
#[allow(unused_variables)]
async fn run_relay_inner(
    cfg: mcpr_tunnel::RelayConfig,
    ready_fd: Option<i32>,
    config_path: String,
) {
    let (app, port) = mcpr_tunnel::build_relay_app(cfg);

    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{port}"))
        .await
        .unwrap_or_else(|e| {
            eprintln!("error: failed to bind relay on port {port}: {e}");
            std::process::exit(1);
        });
    let actual_port = listener.local_addr().unwrap().port();

    // Read daemon PID for watchdog.
    #[cfg(unix)]
    let daemon_pid = daemon::read_pid_file().map(|i| i.pid);

    // Write lockfile.
    #[cfg(unix)]
    if let Err(e) = relay_lock::write_lock(actual_port, &config_path) {
        eprintln!("error: failed to write relay lockfile: {e}");
        std::process::exit(1);
    }

    // Signal readiness to parent (if daemonized).
    #[cfg(unix)]
    if let Some(fd) = ready_fd {
        daemon::signal_ready(fd);
    }

    eprintln!(
        "  {} relay listening on :{actual_port}",
        colored::Colorize::green("mcpr")
    );

    // Shutdown signal.
    let (shutdown_tx, _) = tokio::sync::watch::channel(false);
    let mut shutdown_rx = shutdown_tx.subscribe();

    // Signal handler (SIGTERM + SIGINT).
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

        eprintln!("[mcpr] Received shutdown signal, stopping relay...");
        let _ = shutdown_trigger.send(true);
    });

    // Serve with graceful shutdown.
    let shutdown_for_server = shutdown_tx.subscribe();
    tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let mut rx = shutdown_for_server;
                let _ = rx.changed().await;
            })
            .await
            .expect("Relay server failed");
    });

    // Daemon watchdog: if the daemon dies, shut down this relay.
    #[cfg(unix)]
    if let Some(dpid) = daemon_pid {
        let watchdog_shutdown = shutdown_tx.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(5)).await;
                if !daemon::is_process_alive(dpid) {
                    eprintln!("[mcpr] daemon died, shutting down relay...");
                    let _ = watchdog_shutdown.send(true);
                    break;
                }
            }
        });
    }

    // Wait for shutdown signal.
    let _ = shutdown_rx.changed().await;

    // Clean up lockfile.
    relay_lock::remove_lock();

    eprintln!("[mcpr] Relay shutdown complete.");
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
async fn probe_mcp_upstream(url: &str, client: &reqwest::Client, health_ref: &SharedProxyHealth) {
    let (status, warning) = check_mcp_endpoint(url, client).await;
    let mut h = proxy_health::lock_health(health_ref);
    h.mcp_status = status;
    h.mcp_warning = warning;
}

/// Send an MCP `initialize` request and classify the result.
/// Returns (status, optional warning message).
async fn check_mcp_endpoint(
    url: &str,
    client: &reqwest::Client,
) -> (proxy_health::ConnectionStatus, Option<String>) {
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
            return (
                proxy_health::ConnectionStatus::Disconnected,
                Some(hint.to_string()),
            );
        }
    };

    let status_code = resp.status().as_u16();

    // Auth-protected servers return 401/403 — the server is reachable and likely MCP,
    // we just can't verify with a probe. Mark as connected; the first real client
    // initialize will confirm status.
    if status_code == 401 || status_code == 403 {
        return (
            proxy_health::ConnectionStatus::Connected,
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
                proxy_health::ConnectionStatus::Connected,
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
            return (
                proxy_health::ConnectionStatus::NotMcp,
                Some(hint.to_string()),
            );
        }
    };

    // Check if it's a JSON-RPC 2.0 response
    if parsed.get("jsonrpc").and_then(|v| v.as_str()) != Some("2.0") {
        return (
            proxy_health::ConnectionStatus::NotMcp,
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
                proxy_health::ConnectionStatus::NotMcp,
                Some("JSON-RPC server but 'initialize' method not found.".to_string()),
            );
        }
        return (
            proxy_health::ConnectionStatus::Connected,
            Some(format!("MCP init error: {msg}")),
        );
    }

    // Check for valid initialize result with serverInfo
    if let Some(result) = parsed.get("result") {
        if result.get("serverInfo").is_some() || result.get("capabilities").is_some() {
            // Valid MCP server
            return (proxy_health::ConnectionStatus::Connected, None);
        }
        // Has a result but no serverInfo — might be MCP-ish but unexpected
        return (
            proxy_health::ConnectionStatus::Connected,
            Some("Server responded but missing serverInfo in initialize result.".to_string()),
        );
    }

    // Fallback — got JSON-RPC but couldn't classify
    (proxy_health::ConnectionStatus::Connected, None)
}

/// Periodically check MCP upstream and widget source connectivity.
async fn health_check_loop(proxy: Arc<ProxyState>) {
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .unwrap();

    loop {
        // Check MCP upstream with protocol-level validation
        let (mcp_status, mcp_warning) = check_mcp_endpoint(&proxy.mcp_upstream, &http).await;

        // Discover widgets (reuses shared logic from widgets.rs)
        let names = widgets::discover_widget_names(&proxy).await;
        let widgets_status = if proxy.widget_source.is_none() {
            proxy_health::ConnectionStatus::Unknown
        } else if names.is_empty() {
            proxy_health::ConnectionStatus::Disconnected
        } else {
            proxy_health::ConnectionStatus::Connected
        };
        let widget_count = if names.is_empty() {
            None
        } else {
            Some(names.len())
        };

        {
            let mut h = proxy_health::lock_health(&proxy.health);
            h.mcp_status = mcp_status;
            h.mcp_warning = mcp_warning;
            h.widgets_status = widgets_status;
            h.widget_count = widget_count;
            h.widget_names = names;
        }

        // Emit heartbeat event via the event bus.
        {
            let h = proxy_health::lock_health(&proxy.health);
            proxy
                .event_bus
                .emit(mcpr_core::event::ProxyEvent::Heartbeat(
                    mcpr_core::event::HeartbeatEvent {
                        ts: chrono::Utc::now().timestamp_millis(),
                        proxy: proxy.name.clone(),
                        mcp_status: h.mcp_status.label().to_string(),
                        tunnel_status: h.tunnel_status.label().to_string(),
                        widgets_status: h.widgets_status.label().to_string(),
                        uptime_secs: h.started_at.elapsed().as_secs(),
                        request_count: h.request_count,
                    },
                ));
        }

        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
    }
}
