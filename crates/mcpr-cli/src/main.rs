//! # mcpr (CLI binary)
//!
//! mcpr is a sidecar primitive in the envoy / pgbouncer mold. The launched PID
//! is the proxy itself - your host process supervisor (systemd, Docker,
//! Node `child_process.spawn`, terminal) owns the lifecycle.
//!
//! - **proxy**: `mcpr proxy run <config>` - runs the MCP gateway in the
//!   foreground. SIGTERM drains gracefully.
//!
//! State (the SQLite store) lives under `~/.mcpr/`.

mod cmd;
mod config;
mod logic;

mod render;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use config::{CliAction, GatewayConfig};
use mcpr_core::event::EventManager;
use mcpr_core::event::types::HeartbeatEvent;
use mcpr_core::event::{EventBus, ProxyEvent};
use mcpr_integrations::cloud_client::DEFAULT_CLOUD_INGEST_ENDPOINT;
use mcpr_integrations::sinks::cloud_sink::{CloudSink, CloudSinkConfig};
use mcpr_integrations::store::{Store, StoreConfig, path::resolve_db_path};
use mcpr_integrations::{StderrSink, sinks::SqliteSink};

fn main() {
    let action = config::load();

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
        CliAction::ProxyRun { cfg } => {
            run_gateway_inner(*cfg).await;
        }
        CliAction::Store(cmd) => {
            cmd::handle_store_command(cmd);
        }
    }
}

async fn run_gateway_inner(cfg: GatewayConfig) {
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
        auth: cfg.auth.clone(),
    });

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

    let mut event_manager = EventManager::new();
    event_manager.register(Box::new(StderrSink));
    event_manager.register(Box::new(SqliteSink::new(store, cfg.name.as_str())));

    if let Some(token) = cfg.cloud_token.clone() {
        let endpoint = cfg
            .cloud_endpoint
            .clone()
            .unwrap_or_else(|| DEFAULT_CLOUD_INGEST_ENDPOINT.to_string());
        let cloud_cfg = CloudSinkConfig {
            endpoint,
            token,
            server: cfg.cloud_server.clone(),
            batch_size: cfg.cloud_batch_size.unwrap_or(100),
            flush_interval: std::time::Duration::from_millis(
                cfg.cloud_flush_interval_ms.unwrap_or(5_000),
            ),
            on_flush: None,
        };
        event_manager.register(Box::new(CloudSink::new(cloud_cfg)));
    }

    let event_bus_handler = event_manager.start();
    let event_bus = event_bus_handler.bus.clone();

    let app = match mcpr_core::proxy2::build_app(proxy_cfg, event_bus.clone()).await {
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

    spawn_heartbeat_task(
        event_bus.clone(),
        mcp.clone(),
        actual_port,
        shutdown_tx.subscribe(),
    );

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

    eprintln!("[mcpr] Shutdown complete.");
}

const HEARTBEAT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

fn spawn_heartbeat_task(
    bus: EventBus,
    upstream: String,
    export_port: u16,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(HEARTBEAT_INTERVAL);
        // Skip the immediate first tick so the first heartbeat fires on the
        // 30s boundary rather than at startup before anything is settled.
        ticker.tick().await;
        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    bus.emit(ProxyEvent::Heartbeat(std::sync::Arc::new(HeartbeatEvent {
                        mcp_status: "running".to_string(),
                        tunnel_status: "disabled".to_string(),
                        tunnel_address: None,
                        upstream: upstream.clone(),
                        export_port,
                        ts: chrono::Utc::now(),
                    })));
                }
                _ = shutdown_rx.changed() => break,
            }
        }
    });
}
