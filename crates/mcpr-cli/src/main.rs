//! # mcpr (CLI binary)
//!
//! mcpr is a sidecar primitive in the envoy / pgbouncer mold. The launched PID
//! is the proxy itself — your host process supervisor (systemd, Docker,
//! Node `child_process.spawn`, terminal) owns the lifecycle.
//!
//! - **proxy**: `mcpr proxy run <config>` — runs the MCP gateway in the
//!   foreground. SIGTERM drains gracefully.
//! - **relay**: `mcpr relay run <config>` — tunnel relay server that accepts
//!   WebSocket connections and assigns subdomains. One per machine.
//!
//! All state (lockfiles, config snapshots, sqlite store) lives under `~/.mcpr/`.

mod cmd;
mod config;
mod logic;

mod proxy_lock;
mod relay_lock;
mod render;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use config::{CliAction, GatewayConfig, Mode};
use mcpr_core::event::EventManager;
use mcpr_integrations::store::{Store, StoreConfig, path::resolve_db_path};
use mcpr_integrations::{StderrSink, sinks::SqliteSink};

fn main() {
    let action = config::load();

    if let CliAction::ProxyRun {
        mode: Mode::Gateway(cfg),
        config_content,
        config_path: _,
    } = &action
    {
        let proxy_name = &cfg.name;
        match proxy_lock::check_lock(proxy_name) {
            proxy_lock::LockStatus::Free => {}
            proxy_lock::LockStatus::Stale(_) => {
                proxy_lock::remove_lock(proxy_name);
            }
            proxy_lock::LockStatus::Held(info) => {
                eprintln!(
                    "error: proxy \"{}\" is already running (pid {}).",
                    proxy_name, info.pid
                );
                std::process::exit(1);
            }
        }
        if let Err(e) = proxy_lock::snapshot_config(proxy_name, config_content) {
            eprintln!("error: failed to snapshot config: {e}");
            std::process::exit(1);
        }
    }

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("Failed to create tokio runtime")
        .block_on(async_main(action));
}

async fn async_main(action: CliAction) {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .try_init();

    match action {
        CliAction::Validate(args) => {
            let issues = config::validate_config(args.config.as_deref());
            let has_error = issues.iter().any(|(s, _)| *s == "error");
            render::validate_issues(&issues);
            std::process::exit(if has_error { 1 } else { 0 });
        }
        CliAction::Version => {
            render::version_info();
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
                run_gateway_inner(*cfg, config_path).await;
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
            run_relay_inner(relay_config, config_path).await;
        }
        CliAction::Relay(cmd) => {
            cmd::handle_relay_command(cmd);
        }
    }
}

// Run a proxy to handle requests by using axum
async fn run_gateway_inner(cfg: GatewayConfig, config_path: String) {
    use std::sync::Arc;

    let mcp = match cfg.mcp {
        Some(u) => u,
        None => {
            eprintln!("error: `mcp` is required in mcpr.toml");
            std::process::exit(1);
        }
    };

    // Read the configuration
    let proxy_cfg = Arc::new(mcpr_core::proxy2::proxy_config::ProxyConfig {
        name: cfg.name.clone(),
        mcp: mcp.clone(),
        port: cfg.port,
        csp: cfg.csp,
        max_request_body_size: cfg.max_request_body_size,
        max_response_body_size: cfg.max_response_body_size,
        max_concurrent_upstream: cfg.max_concurrent_upstream,
        connect_timeout: cfg.connect_timeout,
        request_timeout: cfg.request_timeout,
    });

    // Open the local SQLite store. Default path is ~/.mcpr/store.db; the
    // user can override via $MCPR_DB. The store handles its own writer
    // thread and drains on drop, so no explicit shutdown is needed here —
    // when EventBusHandle::shutdown() returns, the sinks Vec drops, the
    // SqliteSink drops, and Store::drop flushes pending events.
    let db_path = match resolve_db_path(None) {
        Some(p) => p,
        None => {
            eprintln!("error: could not determine mcpr data directory ($HOME unset?)");
            std::process::exit(1);
        }
    };
    let store = match Store::open(StoreConfig {
        db_path,
        mcpr_version: env!("CARGO_PKG_VERSION").into(),
    }) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: failed to open store: {e}");
            std::process::exit(1);
        }
    };

    // Setup Event Bus
    let mut event_manager = EventManager::new();
    event_manager.register(Box::new(StderrSink));
    event_manager.register(Box::new(SqliteSink::new(store, cfg.name.as_str())));
    let event_bus_handler = event_manager.start();
    let event_bus = event_bus_handler.bus.clone();

    let app = match mcpr_core::proxy2::build_app(proxy_cfg, event_bus) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("error: failed to build proxy app: {e}");
            std::process::exit(1);
        }
    };

    let bind_port = cfg.port.unwrap_or(3004);
    let listener = match tokio::net::TcpListener::bind(format!("0.0.0.0:{bind_port}")).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("error: failed to bind on port {bind_port}: {e}");
            std::process::exit(1);
        }
    };
    let actual_port = listener.local_addr().unwrap().port();

    #[cfg(unix)]
    if let Err(e) = proxy_lock::write_lock(&cfg.name, actual_port, &config_path) {
        eprintln!("error: failed to write lockfile: {e}");
        std::process::exit(1);
    }

    let public_url = if cfg.tunnel {
        let relay_url = cfg
            .relay_url
            .as_deref()
            .unwrap_or("https://tunnel.mcpr.app");

        let token = match cfg.tunnel_token.as_deref() {
            Some(t) => t,
            None => {
                eprintln!(
                    "  {}: tunnel.enabled = true but no tunnel.token configured. \
                     Register at https://mcpr.app to get one, then set tunnel.token in mcpr.toml.",
                    colored::Colorize::red("error"),
                );
                std::process::exit(1);
            }
        };

        match mcpr_tunnel::start_tunnel_client(
            actual_port,
            relay_url,
            token,
            cfg.tunnel_subdomain.as_deref(),
            StderrTunnelStatus,
        )
        .await
        {
            Ok(url) => Some(url),
            Err(e) => {
                eprintln!(
                    "  {}: failed to connect to relay: {e}",
                    colored::Colorize::red("error"),
                );
                eprintln!("  hint: set tunnel.enabled = false in mcpr.toml to use proxy-only mode");
                std::process::exit(1);
            }
        }
    } else {
        None
    };

    eprintln!(
        "  {} mcpr proxy running on http://localhost:{actual_port} -> {mcp}",
        colored::Colorize::green("ready"),
    );
    if let Some(url) = public_url.as_deref() {
        eprintln!("  {} public URL: {url}", colored::Colorize::green("tunnel"),);
    }

    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);

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
        let _ = shutdown_trigger.send(true);
    });

    let _ = shutdown_rx.changed().await;

    proxy_lock::remove_lock(&cfg.name);
    eprintln!("[mcpr] Shutdown complete.");
}

/// Run the relay server process. Called from `mcpr relay run` only.
/// Always foreground — the launching process owns the PID.
async fn run_relay_inner(cfg: mcpr_tunnel::RelayConfig, config_path: String) {
    let (app, port) = mcpr_tunnel::build_relay_app(cfg);

    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{port}"))
        .await
        .unwrap_or_else(|e| {
            eprintln!("error: failed to bind relay on port {port}: {e}");
            std::process::exit(1);
        });
    let actual_port = listener.local_addr().unwrap().port();

    #[cfg(unix)]
    if let Err(e) = relay_lock::write_lock(actual_port, &config_path) {
        eprintln!("error: failed to write relay lockfile: {e}");
        std::process::exit(1);
    }

    eprintln!(
        "  {} relay listening on :{actual_port}",
        colored::Colorize::green("mcpr")
    );

    let (shutdown_tx, _) = tokio::sync::watch::channel(false);
    let mut shutdown_rx = shutdown_tx.subscribe();

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

    let _ = shutdown_rx.changed().await;

    relay_lock::remove_lock();

    eprintln!("[mcpr] Relay shutdown complete.");
}

struct StderrTunnelStatus;

impl mcpr_tunnel::TunnelStatusCallback for StderrTunnelStatus {
    fn on_connected(&self, url: &str) {
        eprintln!("[mcpr] tunnel connected: {url}");
    }
    fn on_disconnected(&self) {
        eprintln!("[mcpr] tunnel disconnected");
    }
    fn on_evicted(&self) {
        eprintln!("[mcpr] tunnel evicted by relay");
    }
}
