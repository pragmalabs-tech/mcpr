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
//!
//! NOTE: the gateway runtime is mid-rewrite onto `mcpr_core::proxy2`. Until
//! that wiring lands, `mcpr proxy run` calls `todo!()`. The legacy `proxy`
//! module is parked in tree as reference and not used by the CLI.

mod cmd;
mod config;
mod logic;

#[allow(dead_code)]
mod proxy_lock;
mod relay_lock;
mod render;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use config::{CliAction, GatewayConfig, Mode};

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
        CliAction::Update => {
            eprintln!("Updating mcpr to the latest version...");
            let status = std::process::Command::new("sh")
                .args(["-c", "curl -fsSL https://mcpr.app/install.sh | sh"])
                .status();
            match status {
                Ok(s) if s.success() => {
                    eprintln!(
                        "Updated. Restart any running proxies via your supervisor (systemd / Docker / Node)."
                    );
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

    let app = match mcpr_core::proxy2::build_app(proxy_cfg) {
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

    eprintln!(
        "  {} mcpr proxy running on http://localhost:{actual_port} -> {mcp}",
        colored::Colorize::green("ready"),
    );

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
