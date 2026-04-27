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
//! ## Module Structure
//!
//! Three-layer architecture: **render** (terminal output), **logic** (data operations),
//! **cmd** (thin command dispatch).
//!
//! ```text
//! mcpr-cli/src/
//! +-- main.rs           # Entry point, gateway runtime
//! +-- state.rs          # AppState (axum host container wrapping ProxyState)
//! +-- proxy.rs          # Axum fallback handler → mcpr_core pipeline
//! +-- config.rs         # CLI args (clap), TOML config, subcommands
//! +-- render.rs         # All terminal output — tables, colors, formatting
//! +-- admin.rs          # Health/readiness admin server
//! +-- proxy_lock.rs     # Per-proxy lockfiles under ~/.mcpr/proxies/
//! +-- relay_lock.rs     # Singleton relay lockfile under ~/.mcpr/relay/
//! +-- logic/            # Core business logic (no printing)
//! |   +-- proxy.rs      #   Proxy lifecycle (stop, reload, list, delete)
//! |   +-- relay.rs      #   Relay lifecycle (stop, status)
//! |   +-- query.rs      #   DB engine, time parsing, threshold parsing
//! +-- cmd/              # Thin command handlers (logic → render)
//!     +-- proxy.rs      #   Proxy lifecycle commands
//!     +-- relay.rs      #   Relay lifecycle commands
//!     +-- observe.rs    #   Observability commands (logs, stats, sessions, …)
//!     +-- store.rs      #   Store maintenance commands
//!     +-- setup.rs      #   Interactive setup flow (mcpr proxy setup)
//! ```
//!
//! The proxy engine (pipeline, middleware, `ProxyState`, forwarding,
//! rewrite, SSE, health) lives in `mcpr_core::proxy`. This binary
//! assembles a `ProxyState` + `ProxyPipeline` at boot, wires them into
//! axum via `AppState`, and drives every request through
//! `AppState::pipeline.run`.
//!
//! The event bus (routes `ProxyEvent` to sinks) lives in `mcpr_core::event`;
//! sink implementations (stderr, sqlite, cloud) live in `mcpr_integrations`.
//! This binary just registers them via `EventManager`.

mod admin;
mod cmd;
mod config;
mod logic;

mod proxy;
#[allow(dead_code)]
mod proxy_lock;
mod relay_lock;
mod render;
mod state;

// Use mimalloc as the process-wide allocator. Consistently faster than
// the system default on request paths that allocate per request
// (HeaderMaps, Bytes, serde_json buffers).
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use mcpr_core::proxy::forwarding::UpstreamClient;

use axum::Router;
use axum::extract::DefaultBodyLimit;
use tower_http::cors::{Any, CorsLayer};

use config::{CliAction, GatewayConfig, Mode};
use mcpr_core::protocol::schema_manager::{MemorySchemaStore, SchemaManager};
use mcpr_core::protocol::session::MemorySessionStore;
use mcpr_core::proxy::ProxyState;
use mcpr_core::proxy::RewriteConfig;
use mcpr_core::proxy::health::{self as proxy_health, SharedProxyHealth};
use proxy::proxy_routes;
use state::AppState;

/// Global drain flag — set to true when graceful shutdown begins.
static IS_DRAINING: AtomicBool = AtomicBool::new(false);

pub const DEFAULT_MAX_REQUEST_BODY_SIZE: usize = 5 * 1024 * 1024;
pub const DEFAULT_MAX_RESPONSE_BODY_SIZE: usize = 10 * 1024 * 1024;
/// Hard ceiling on in-flight upstream requests (semaphore). Rarely hit
/// in real workloads; here to prevent runaway resource use if a client
/// flood outpaces upstream capacity.
pub const DEFAULT_MAX_CONCURRENT_UPSTREAM: usize = 100;
/// Idle-connection pool size per upstream host. Sized for the p99 < 1 ms
/// sweet spot we measured in `benches/scripts/scenarios/perf/stress.sh`:
/// throughput scales cleanly from 1 → ~30 concurrent with p99 ≤ 1 ms,
/// saturates around 50; 64 covers the whole comfortable zone with
/// headroom. Bumping higher buys you a few extra % of ceiling
/// throughput but costs memory + fds per idle connection with minimal
/// latency benefit.
pub const DEFAULT_MAX_IDLE_UPSTREAM_PER_HOST: usize = 64;
pub const DEFAULT_CONNECT_TIMEOUT_SECS: u64 = 5;
pub const DEFAULT_REQUEST_TIMEOUT_SECS: u64 = 30;

pub fn build_app(app_state: AppState) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any)
        .expose_headers(Any);

    let max_request = app_state.proxy.max_request_body;
    let max_concurrent = app_state.proxy.max_concurrent_upstream;
    let request_timeout = app_state.proxy.upstream.request_timeout;

    let app: Router<AppState> = Router::new();
    let app = proxy_routes(app);
    app.with_state(app_state)
        // Body cap first — reject oversized requests before they consume
        // a concurrency slot or start a timeout budget.
        .layer(DefaultBodyLimit::max(max_request))
        // Concurrency cap. For a 1:1 proxy, inbound concurrency ==
        // upstream concurrency in steady state.
        .layer(tower::limit::ConcurrencyLimitLayer::new(max_concurrent))
        // Per-request timeout — covers the buffered path end-to-end.
        // Streaming responses keep their own per-call reqwest timeout
        // (see `forward_request`) since this layer can't see mid-stream
        // stalls.
        .layer(tower_http::timeout::TimeoutLayer::with_status_code(
            axum::http::StatusCode::GATEWAY_TIMEOUT,
            request_timeout,
        ))
        // Operator tracing for HTTP metadata: method, path, status,
        // latency. Orthogonal to the `RequestEvent` bus — tracing is
        // for log aggregation, `RequestEvent` is the observability
        // product surface.
        .layer(tower_http::trace::TraceLayer::new_for_http())
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

/// Entry point. Pre-tokio work is limited to config snapshotting + lockfile
/// conflict checks for `mcpr proxy run`; the proxy then runs in the foreground
/// of the launching process (terminal, systemd, Docker, Node `child_process`).
/// There is no daemonization — the host process owns the lifecycle.
fn main() {
    let action = config::load();

    // Pre-tokio work for `mcpr proxy run`: write the lockfile-conflict guard
    // and snapshot the config so reload / stop can find it.
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
                eprintln!(
                    "  Send SIGTERM (`mcpr proxy stop {}`) to stop it first,",
                    proxy_name
                );
                eprintln!(
                    "  or `mcpr proxy reload {} --config <path>` for a zero-downtime CSP reload.",
                    proxy_name
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
    // Install the global tracing subscriber once per process. Honors
    // `RUST_LOG` (e.g. `RUST_LOG=info` surfaces middleware registration
    // + tower trace spans). Defaults to `warn` when unset — operator
    // CLI output stays terse by default.
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

/// Run a proxy gateway process. Called from `mcpr proxy run` only.
/// Always foreground — the launching process owns the PID. Writes a
/// lockfile so `mcpr proxy stop / reload / list` can find it from another
/// terminal; SIGTERM / SIGINT trigger graceful drain.
async fn run_gateway_inner(cfg: GatewayConfig, config_path: String) {
    // Preserve the resolved startup config for the live-reload handler so
    // SIGHUP can diff an incoming snapshot against the config actually in
    // effect. Cloned up-front because the fn body moves fields out of `cfg`.
    let reload_applied_cfg = Arc::new(tokio::sync::Mutex::new(cfg.clone()));

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

    // Bind listener — in tunnel mode with no explicit port, use port 0 (OS picks random).
    // In proxy-only mode (no tunnel), default to 3000 if not specified.
    // Default port: 3000 for proxy-only mode, 0 (OS picks) for tunnel mode.
    let bind_port = cfg.port.unwrap_or(if cfg.tunnel { 0 } else { 3000 });
    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{bind_port}"))
        .await
        .expect("Failed to bind");
    let actual_port = listener.local_addr().unwrap().port();

    // Write lockfile so `mcpr proxy stop / reload / list` can find this process.
    #[cfg(unix)]
    if let Err(e) = proxy_lock::write_lock(&proxy_name, actual_port, &config_path) {
        eprintln!("error: failed to write lockfile: {e}");
        std::process::exit(1);
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

    let (proxy_url_for_rewrite, proxy_domain) =
        resolve_proxy_origin(cfg.csp.domain.as_deref(), &public_url);

    let rewrite_config = RewriteConfig {
        proxy_url: proxy_url_for_rewrite,
        proxy_domain,
        mcp_upstream: mcp.clone(),
        csp: cfg.csp.clone(),
    };

    let connect_timeout =
        Duration::from_secs(cfg.connect_timeout.unwrap_or(DEFAULT_CONNECT_TIMEOUT_SECS));
    let request_timeout =
        Duration::from_secs(cfg.request_timeout.unwrap_or(DEFAULT_REQUEST_TIMEOUT_SECS));

    // Upstream connection pool sized to the sweet-spot concurrency
    // (measured: p99 < 1 ms up through ~30 simultaneous clients, ~50
    // saturates). 64 covers that comfortably. The separate semaphore
    // is a safety cap on in-flight requests, not a steady-state
    // target — most deployments never hit it. See
    // `DEFAULT_MAX_IDLE_UPSTREAM_PER_HOST` for the rationale.
    let max_concurrent = cfg
        .max_concurrent_upstream
        .unwrap_or(DEFAULT_MAX_CONCURRENT_UPSTREAM);
    let upstream = UpstreamClient {
        http_client: reqwest::Client::builder()
            .connect_timeout(connect_timeout)
            .pool_idle_timeout(Duration::from_secs(90))
            .pool_max_idle_per_host(DEFAULT_MAX_IDLE_UPSTREAM_PER_HOST)
            .build()
            .expect("Failed to build HTTP client"),
        request_timeout,
    };

    // Build event sinks — one pipeline, multiple destinations.
    let mut event_manager = mcpr_core::event::EventManager::new();

    // 1. Stderr sink — real-time console output.
    event_manager.register(Box::new(mcpr_integrations::StderrSink::new(
        cfg.runtime.log_format,
    )));

    // 2. SQLite sink — local storage for CLI queries.
    // Keep the db_path around after opening so we can hydrate the
    // SchemaManager from it below — sinks own the Store, but the
    // proxy bootstrap also needs read access to warm schema state.
    let mut sqlite_db_path: Option<std::path::PathBuf> = None;
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
                sqlite_db_path = Some(db_path);
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
        max_concurrent_upstream: max_concurrent,
        rewrite_config: rewrite_config.into_swap(),
        sessions: MemorySessionStore::new(),
        schema_manager: Arc::new(SchemaManager::new(
            proxy_name.clone(),
            MemorySchemaStore::new(),
        )),
        health: proxy_health_ref.clone(),
        event_bus: event_bus_handle.bus.clone(),
    });
    // Pipeline construction itself emits `info!` registration logs for
    // each middleware — surface them to the operator via tracing
    // (`RUST_LOG=info`), not as a secondary eprintln! banner.
    let pipeline = Arc::new(mcpr_core::proxy::build_default_pipeline(
        proxy.rewrite_config.clone(),
    ));
    let app_state = AppState {
        proxy: proxy.clone(),
        pipeline,
    };

    // Hydrate SchemaManager from disk so dashboards show the last
    // captured schema immediately after restart (instead of an empty
    // server until the first real client call). Passive design: only
    // reads local SQLite, never probes the upstream.
    if let Some(ref db_path) = sqlite_db_path {
        hydrate_schema_manager_from_sqlite(&proxy.schema_manager, &proxy.name, db_path).await;
    }

    // Initial connectivity probe — warn early if the MCP URL seems wrong
    probe_mcp_upstream(&mcp, &upstream.http_client, &proxy_health_ref).await;

    let health_proxy = proxy.clone();

    let app = build_app(app_state);

    render::log_startup(
        &proxy_health_ref,
        actual_port,
        &public_url,
        &mcp,
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

    // Spawn health check task: periodically probe MCP upstream status
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

    // SIGHUP → hot-reload CSP from the on-disk snapshot.
    //
    // Only CSP is live-swappable today (via the Arc<ArcSwap<RewriteConfig>>
    // already wired into every rewrite path). If anything else in the config
    // differs from the currently-applied snapshot, the reload is rejected and
    // the proxy keeps running on the old config — the operator gets a clear
    // log line naming which field needs a full `mcpr proxy restart`.
    #[cfg(unix)]
    {
        let reload_proxy = proxy.clone();
        let reload_name = proxy_name_for_shutdown.clone();
        let reload_applied = reload_applied_cfg.clone();
        let reload_public_url = public_url.clone();
        tokio::spawn(async move {
            use tokio::signal::unix::{SignalKind, signal};
            let mut sighup = match signal(SignalKind::hangup()) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("[mcpr] reload: SIGHUP handler unavailable: {e}");
                    return;
                }
            };
            while sighup.recv().await.is_some() {
                handle_reload(
                    &reload_name,
                    &reload_applied,
                    &reload_proxy,
                    &reload_public_url,
                )
                .await;
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

/// Compute the rewrite-time `(proxy_url, proxy_domain)` from the operator's
/// `csp.domain` declaration plus the bound public URL (tunnel or local).
///
/// Precedence:
///   1. `csp.domain` in mcpr.toml (operator-declared)
///   2. The public URL the proxy was bound to (tunnel URL or `http://localhost:PORT`)
///   3. Local-only dev — `proxy_url` stays as `http://localhost:*` for internal
///      wiring, but `proxy_domain` is empty so widget-domain rewriting is
///      suppressed (no `localhost` leaks into a submitted template).
fn resolve_proxy_origin(csp_domain: Option<&str>, public_url: &str) -> (String, String) {
    if let Some(domain) = csp_domain {
        let bare = domain
            .trim_start_matches("https://")
            .trim_start_matches("http://")
            .trim_end_matches('/')
            .to_string();
        return (format!("https://{bare}"), bare);
    }
    let bare = public_url
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/')
        .to_string();
    let domain = if bare.contains("localhost") || bare.contains("127.0.0.1") {
        String::new()
    } else {
        bare
    };
    (public_url.to_string(), domain)
}

/// Apply a SIGHUP-triggered config reload.
///
/// Pairs the SIGHUP with the request nonce the CLI just wrote, performs the
/// reload, and writes the outcome back so the waiting CLI can print success
/// or the rejection reason. SIGHUPs that arrive without a request file
/// (e.g. external `kill -HUP`) still run the reload; they just don't get a
/// result file written.
async fn handle_reload(
    proxy_name: &str,
    applied: &tokio::sync::Mutex<GatewayConfig>,
    proxy: &Arc<ProxyState>,
    public_url: &str,
) {
    let nonce = proxy_lock::read_reload_request(proxy_name).ok();

    let outcome = apply_reload(proxy_name, applied, proxy, public_url).await;

    match &outcome {
        Ok(()) => eprintln!("[mcpr] reload: applied"),
        Err(reason) => eprintln!("[mcpr] reload: rejected — {reason}"),
    }

    if let Some(n) = nonce {
        let (status, message) = match &outcome {
            Ok(()) => (proxy_lock::ReloadStatus::Applied, String::from("ok")),
            Err(reason) => (proxy_lock::ReloadStatus::Rejected, reason.clone()),
        };
        if let Err(e) = proxy_lock::write_reload_result(proxy_name, n, status, &message) {
            eprintln!("[mcpr] reload: failed to write result file: {e}");
        }
    }
}

/// Pure reload step: parse the snapshot, reject on unsafe changes, and swap
/// in a freshly-built `RewriteConfig`. Returns `Ok(())` when the live config
/// has been swapped, or `Err(reason)` when the proxy must keep running on
/// the old config.
///
/// `proxy_url`/`proxy_domain` are recomputed from the new snapshot's
/// `csp.domain` — reusing the old values would silently drop a domain change
/// even though the rest of the CSP block updated. `mcp_upstream` is kept
/// from the running config because `mcp` is in the unsafe-changes set, so
/// it cannot have moved.
async fn apply_reload(
    proxy_name: &str,
    applied: &tokio::sync::Mutex<GatewayConfig>,
    proxy: &Arc<ProxyState>,
    public_url: &str,
) -> Result<(), String> {
    let snapshot_path = proxy_lock::config_snapshot_path(proxy_name);
    let new_cfg = config::load_gateway_from_path(&snapshot_path)
        .map_err(|e| format!("snapshot parse failed: {e}"))?;

    let mut applied_guard = applied.lock().await;
    let changed = applied_guard.reload_unsafe_changes(&new_cfg);
    if !changed.is_empty() {
        return Err(format!(
            "fields require restart: {}. Use `mcpr proxy restart {} --config <path>` to apply.",
            changed.join(", "),
            proxy_name,
        ));
    }

    let current = proxy.rewrite_config.load();
    let new_rewrite = build_reload_rewrite_config(&new_cfg, public_url, &current.mcp_upstream);
    drop(current);
    proxy.rewrite_config.store(Arc::new(new_rewrite));
    *applied_guard = new_cfg;
    Ok(())
}

/// Build the `RewriteConfig` that the SIGHUP path swaps in. Pure: no IO,
/// no locks. Recomputes the public origin from the new snapshot's
/// `csp.domain` so a domain change actually takes effect — the previous
/// code reused the boot-time origin and silently dropped any update there.
/// `mcp_upstream` is carried from the running config because `mcp` is in
/// the unsafe-changes set and so cannot have moved.
fn build_reload_rewrite_config(
    new_cfg: &GatewayConfig,
    public_url: &str,
    mcp_upstream: &str,
) -> RewriteConfig {
    let (proxy_url, proxy_domain) = resolve_proxy_origin(new_cfg.csp.domain.as_deref(), public_url);
    RewriteConfig {
        proxy_url,
        proxy_domain,
        mcp_upstream: mcp_upstream.to_string(),
        csp: new_cfg.csp.clone(),
    }
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

/// Seed `SchemaManager` from the `server_schema` table so dashboards
/// reflect the last captured schema immediately after a restart.
///
/// Passive: reads local SQLite only, never contacts the upstream. Runs
/// once at startup, O(5) queries (one per schema method). Any I/O
/// failure logs a warning and is otherwise swallowed — a missing or
/// corrupt store should never prevent the proxy from starting.
async fn hydrate_schema_manager_from_sqlite(
    manager: &mcpr_core::protocol::schema_manager::SchemaManager<
        mcpr_core::protocol::schema_manager::MemorySchemaStore,
    >,
    proxy_name: &str,
    db_path: &std::path::Path,
) {
    use mcpr_core::protocol::schema_manager::{SchemaVersion, SchemaVersionId};
    use mcpr_integrations::store::QueryEngine;

    let engine = match QueryEngine::open(db_path) {
        Ok(e) => e,
        Err(e) => {
            eprintln!(
                "  {}: schema hydration skipped — query engine open failed: {e}",
                colored::Colorize::yellow("warn"),
            );
            return;
        }
    };

    let methods = [
        "initialize",
        "tools/list",
        "resources/list",
        "resources/templates/list",
        "prompts/list",
    ];

    let mut hydrated = 0usize;
    for method in methods {
        let row = match engine.latest_schema_row(proxy_name, method) {
            Ok(Some(r)) => r,
            Ok(None) => continue,
            Err(e) => {
                eprintln!(
                    "  {}: schema hydration query failed for {method}: {e}",
                    colored::Colorize::yellow("warn"),
                );
                continue;
            }
        };
        let Ok(payload): Result<serde_json::Value, _> = serde_json::from_str(&row.payload) else {
            continue;
        };
        let version = SchemaVersion {
            // The 16-hex id is a deterministic prefix of the full hash,
            // so it reconstructs correctly on restart.
            id: SchemaVersionId(row.schema_hash.chars().take(16).collect()),
            upstream_id: proxy_name.to_string(),
            method: method.to_string(),
            // Version numbering is per-process; resetting to 1 at
            // restart is acceptable because external consumers dedupe
            // by content_hash, not by version number.
            version: 1,
            payload: std::sync::Arc::new(payload),
            content_hash: row.schema_hash,
            captured_at: chrono::DateTime::from_timestamp_millis(row.captured_at)
                .unwrap_or_else(chrono::Utc::now),
        };
        manager.preload(version).await;
        hydrated += 1;
    }

    if hydrated > 0 {
        eprintln!(
            "  {} hydrated {hydrated} schema method(s) from {}",
            colored::Colorize::dimmed("schema"),
            db_path.display(),
        );
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

/// Periodically check MCP upstream connectivity.
async fn health_check_loop(proxy: Arc<ProxyState>) {
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .unwrap();

    loop {
        // Check MCP upstream with protocol-level validation
        let (mcp_status, mcp_warning) = check_mcp_endpoint(&proxy.mcp_upstream, &http).await;

        {
            let mut h = proxy_health::lock_health(&proxy.health);
            h.mcp_status = mcp_status;
            h.mcp_warning = mcp_warning;
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
                        uptime_secs: h.started_at.elapsed().as_secs(),
                        request_count: h.request_count,
                    },
                ));
        }

        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
    }
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;
    use config::{LogFormat, RuntimeOptions};
    use mcpr_core::proxy::csp::{CspConfig, WidgetScoped};

    // ── helpers ──────────────────────────────────────────────

    fn gateway_config() -> GatewayConfig {
        GatewayConfig {
            name: "test".into(),
            mcp: Some("http://localhost:9000".into()),
            port: Some(3000),
            csp: CspConfig::default(),
            relay_url: Some("https://tunnel.mcpr.app".into()),
            tunnel_token: None,
            tunnel_subdomain: None,
            tunnel: false,
            max_request_body_size: None,
            max_response_body_size: None,
            max_concurrent_upstream: None,
            connect_timeout: None,
            request_timeout: None,
            cloud_token: None,
            cloud_server: None,
            cloud_endpoint: None,
            cloud_batch_size: None,
            cloud_flush_interval_ms: None,
            runtime: RuntimeOptions {
                drain_timeout: 30,
                log_format: LogFormat::Json,
                admin_bind: "127.0.0.1:9901".into(),
            },
        }
    }

    // ── resolve_proxy_origin ─────────────────────────────────

    #[test]
    fn resolve_proxy_origin__csp_domain_takes_precedence() {
        let (url, domain) =
            resolve_proxy_origin(Some("example.com"), "https://abc.tunnel.mcpr.app");
        assert_eq!(url, "https://example.com");
        assert_eq!(domain, "example.com");
    }

    #[test]
    fn resolve_proxy_origin__csp_domain_strips_https_scheme() {
        let (url, domain) =
            resolve_proxy_origin(Some("https://example.com"), "https://other.example.com");
        assert_eq!(url, "https://example.com");
        assert_eq!(domain, "example.com");
    }

    #[test]
    fn resolve_proxy_origin__csp_domain_strips_http_scheme() {
        let (url, domain) =
            resolve_proxy_origin(Some("http://example.com"), "https://other.example.com");
        assert_eq!(url, "https://example.com");
        assert_eq!(domain, "example.com");
    }

    #[test]
    fn resolve_proxy_origin__csp_domain_strips_trailing_slash() {
        let (url, domain) =
            resolve_proxy_origin(Some("https://example.com/"), "https://other.example.com");
        assert_eq!(url, "https://example.com");
        assert_eq!(domain, "example.com");
    }

    #[test]
    fn resolve_proxy_origin__falls_back_to_public_url() {
        let (url, domain) = resolve_proxy_origin(None, "https://abc.tunnel.mcpr.app");
        assert_eq!(url, "https://abc.tunnel.mcpr.app");
        assert_eq!(domain, "abc.tunnel.mcpr.app");
    }

    #[test]
    fn resolve_proxy_origin__localhost_yields_empty_domain() {
        let (url, domain) = resolve_proxy_origin(None, "http://localhost:3000");
        assert_eq!(url, "http://localhost:3000");
        assert_eq!(domain, "");
    }

    #[test]
    fn resolve_proxy_origin__loopback_ip_yields_empty_domain() {
        let (url, domain) = resolve_proxy_origin(None, "http://127.0.0.1:3000");
        assert_eq!(url, "http://127.0.0.1:3000");
        assert_eq!(domain, "");
    }

    #[test]
    fn resolve_proxy_origin__csp_domain_overrides_localhost_public_url() {
        let (url, domain) =
            resolve_proxy_origin(Some("custom.example.com"), "http://localhost:3000");
        assert_eq!(url, "https://custom.example.com");
        assert_eq!(domain, "custom.example.com");
    }

    // ── build_reload_rewrite_config ──────────────────────────
    //
    // These guard the regression we just fixed: previously the SIGHUP path
    // reused `proxy_url`/`proxy_domain` from the running config and silently
    // dropped any change to `csp.domain`. The expectation now is that the
    // new origin is recomputed from the *new* config every time.

    #[test]
    fn build_reload_rewrite_config__picks_up_new_csp_domain() {
        let mut cfg = gateway_config();
        cfg.csp.domain = Some("new.example.com".into());

        let r = build_reload_rewrite_config(&cfg, "https://stale.tunnel.app", "http://up:9000");

        assert_eq!(r.proxy_url, "https://new.example.com");
        assert_eq!(r.proxy_domain, "new.example.com");
    }

    #[test]
    fn build_reload_rewrite_config__no_csp_domain_falls_back_to_public_url() {
        let cfg = gateway_config();
        let r = build_reload_rewrite_config(&cfg, "https://abc.tunnel.mcpr.app", "http://up:9000");
        assert_eq!(r.proxy_url, "https://abc.tunnel.mcpr.app");
        assert_eq!(r.proxy_domain, "abc.tunnel.mcpr.app");
    }

    #[test]
    fn build_reload_rewrite_config__preserves_csp_block() {
        let mut cfg = gateway_config();
        cfg.csp.widgets.push(WidgetScoped {
            match_pattern: "ui://widget/test".into(),
            ..Default::default()
        });

        let r = build_reload_rewrite_config(&cfg, "https://abc.tunnel.mcpr.app", "http://up:9000");

        assert_eq!(r.csp.widgets.len(), 1);
        assert_eq!(r.csp.widgets[0].match_pattern, "ui://widget/test");
    }

    #[test]
    fn build_reload_rewrite_config__keeps_mcp_upstream_arg() {
        // `mcp` is in the unsafe-changes set, so reload always carries the
        // boot-time string in via the `mcp_upstream` arg rather than reading
        // it from the new snapshot.
        let cfg = gateway_config();
        let r = build_reload_rewrite_config(
            &cfg,
            "https://abc.tunnel.mcpr.app",
            "http://boot-upstream:9000",
        );
        assert_eq!(r.mcp_upstream, "http://boot-upstream:9000");
    }
}
