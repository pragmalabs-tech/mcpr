mod config;
mod display;
mod jsonrpc;
mod onboarding;
mod proxy;
mod relay;
mod rewrite;
mod session;
mod tui;
mod tunnel;
mod widgets;

use std::sync::Arc;
use tokio::sync::RwLock;

use axum::Router;
use axum::extract::DefaultBodyLimit;
use tower_http::cors::{Any, CorsLayer};

use config::{GatewayConfig, Mode};
use display::log_startup;
use proxy::proxy_routes;
use rewrite::RewriteConfig;
use session::MemorySessionStore;
use tui::SharedTuiState;
use widgets::WidgetSource;

pub const DEFAULT_MAX_BODY_SIZE: usize = 5 * 1024 * 1024;

pub fn build_app(state: AppState) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any)
        .expose_headers(Any);

    let max_body = state.max_response_body;

    let app: Router<AppState> = Router::new();
    let app = proxy_routes(app);
    app.with_state(state)
        .layer(DefaultBodyLimit::max(max_body))
        .layer(cors)
}

#[derive(Clone)]
pub struct AppState {
    pub mcp_upstream: String,
    pub widget_source: Option<WidgetSource>,
    pub rewrite_config: Arc<RwLock<RewriteConfig>>,
    pub http_client: reqwest::Client,
    pub tui_state: SharedTuiState,
    pub sessions: MemorySessionStore,
    pub max_response_body: usize,
}

#[tokio::main]
async fn main() {
    match config::load() {
        Mode::Relay(cfg) => {
            relay::start_relay(cfg).await;
        }
        Mode::Gateway(cfg) => {
            run_gateway(cfg).await;
        }
    }
}

async fn run_gateway(cfg: GatewayConfig) {
    let tui_state = tui::new_shared_state();

    let mcp = cfg.mcp.expect("mcp is required in mcpr.toml or --mcp");

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
                Ok((token, subdomain)) => {
                    let save_path = config_path
                        .clone()
                        .unwrap_or_else(|| std::env::current_dir().unwrap().join("mcpr.toml"));
                    // Create the file if it doesn't exist so save_tunnel_config can read+update it
                    if !save_path.exists() {
                        let _ = std::fs::write(&save_path, "");
                    }
                    GatewayConfig::save_tunnel_config(&save_path, &token, &subdomain);
                    eprintln!(
                        "\n  {} We sent a verification link to your email.",
                        colored::Colorize::yellow("!"),
                    );
                    eprintln!(
                        "  {} Verify to keep '{}' permanently — it's reserved for 72 hours.\n",
                        colored::Colorize::yellow("!"),
                        subdomain,
                    );
                    (token, Some(subdomain))
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

        tui_state.lock().unwrap().tunnel_status = tui::ConnectionStatus::Connecting;

        match tunnel::start_tunnel_client(
            actual_port,
            relay_url,
            &token,
            desired_subdomain.as_deref(),
            tui_state.clone(),
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

    let max_body = cfg.max_body_size.unwrap_or(DEFAULT_MAX_BODY_SIZE);
    let state = AppState {
        mcp_upstream: mcp.clone(),
        widget_source,
        rewrite_config: Arc::new(RwLock::new(rewrite_config)),
        http_client: reqwest::Client::new(),
        tui_state: tui_state.clone(),
        sessions: MemorySessionStore::new(),
        max_response_body: max_body,
    };

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

    // Spawn the axum server as a background task
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("Server failed");
    });

    // Spawn health check task: periodically probe MCP + widgets status
    {
        tokio::spawn(async move {
            health_check_loop(health_state).await;
        });
    }

    // Run the TUI on a blocking thread (it reads stdin)
    let tui_handle = tokio::task::spawn_blocking(move || {
        tui::run(tui_state, tui_sessions).expect("TUI failed");
    });

    tui_handle.await.unwrap();
}

/// Periodically check MCP upstream and widget source connectivity.
async fn health_check_loop(app_state: AppState) {
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .unwrap();

    loop {
        // Check MCP upstream
        let mcp_status = match http.get(&app_state.mcp_upstream).send().await {
            Ok(_) => tui::ConnectionStatus::Connected,
            Err(_) => tui::ConnectionStatus::Disconnected,
        };

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
            s.widgets_status = widgets_status;
            s.widget_count = widget_count;
            s.widget_names = names;
        }

        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
    }
}
