use std::sync::atomic::Ordering;

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;

use crate::IS_DRAINING;
use mcpr_core::proxy::health::{ConnectionStatus, SharedProxyHealth};

#[derive(Clone)]
struct AdminState {
    health: SharedProxyHealth,
}

/// Start the admin API server on the given bind address.
pub async fn start_admin_server(
    bind: &str,
    health: SharedProxyHealth,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    let state = AdminState { health };

    let app = Router::new()
        .route("/healthz", get(healthz))
        .route("/ready", get(ready))
        .route("/version", get(version))
        .with_state(state);

    let listener = match tokio::net::TcpListener::bind(bind).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!(
                "[mcpr] {}: failed to bind admin server on {bind}: {e}",
                colored::Colorize::yellow("warn"),
            );
            return;
        }
    };

    eprintln!(
        "  {} admin API listening on {bind}",
        colored::Colorize::dimmed("admin"),
    );

    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let _ = shutdown.changed().await;
        })
        .await
        .expect("Admin server failed");
}

/// Liveness probe — always 200 unless shutting down.
async fn healthz() -> impl IntoResponse {
    if IS_DRAINING.load(Ordering::SeqCst) {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(serde_json::json!({"status": "shutting_down"})),
        );
    }
    (
        StatusCode::OK,
        axum::Json(serde_json::json!({"status": "ok"})),
    )
}

/// Readiness probe — 503 while draining or MCP upstream disconnected.
async fn ready(State(state): State<AdminState>) -> impl IntoResponse {
    if IS_DRAINING.load(Ordering::SeqCst) {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(serde_json::json!({
                "status": "not_ready",
                "reason": "draining"
            })),
        );
    }

    let mcp_connected = {
        let h = mcpr_core::proxy::lock_health(&state.health);
        matches!(h.mcp_status, ConnectionStatus::Connected)
    };

    if !mcp_connected {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(serde_json::json!({
                "status": "not_ready",
                "reason": "mcp_upstream_disconnected"
            })),
        );
    }

    (
        StatusCode::OK,
        axum::Json(serde_json::json!({"status": "ready"})),
    )
}

/// Version endpoint.
async fn version() -> impl IntoResponse {
    axum::Json(serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
    }))
}
