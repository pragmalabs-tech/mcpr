//! Proxy runtime + host container state.
//!
//! [`ProxyState`] is everything one running proxy instance needs to serve
//! traffic — upstream client, rewrite config, sessions, schema manager, per-
//! proxy health, event bus handle. Request handlers take `Arc<ProxyState>`.
//!
//! [`AppState`] is the host-level container that owns proxies. Today it
//! wraps a single proxy; future phases grow this to a map keyed by proxy
//! name once multi-proxy-per-process lands.

use std::sync::Arc;
use tokio::sync::RwLock;

use axum::extract::FromRef;
use mcpr_core::event::EventBus;
use mcpr_core::protocol::schema_manager::{MemorySchemaStore, SchemaManager};
use mcpr_core::protocol::session::MemorySessionStore;
use mcpr_core::proxy::RewriteConfig;
use mcpr_core::proxy::forwarding::UpstreamClient;
use mcpr_core::proxy::health::SharedProxyHealth;

use crate::widgets::WidgetSource;

/// Everything one running proxy needs to serve a request end-to-end.
pub struct ProxyState {
    /// Proxy name used to tag events and look up per-proxy resources.
    pub name: String,

    // ── forwarding ──
    pub mcp_upstream: String,
    pub upstream: UpstreamClient,
    pub max_request_body: usize,
    pub max_response_body: usize,

    // ── response shaping ──
    pub rewrite_config: Arc<RwLock<RewriteConfig>>,
    pub widget_source: Option<WidgetSource>,

    // ── runtime tracking ──
    pub sessions: MemorySessionStore,
    pub schema_manager: Arc<SchemaManager<MemorySchemaStore>>,

    // ── per-proxy health display + tunnel callbacks ──
    pub health: SharedProxyHealth,

    // ── observability (cloned handle, cheap to clone) ──
    pub event_bus: EventBus,
}

/// Host-level container owning the proxy instance(s) this process runs.
///
/// Cloneable because axum `with_state` requires `Clone`. Cloning is cheap —
/// only `Arc` bumps.
#[derive(Clone)]
pub struct AppState {
    pub proxy: Arc<ProxyState>,
}

/// Axum extractor glue: handlers that only need the proxy can write
/// `State<Arc<ProxyState>>` and axum will pull it out of `AppState`.
impl FromRef<AppState> for Arc<ProxyState> {
    fn from_ref(app: &AppState) -> Arc<ProxyState> {
        app.proxy.clone()
    }
}
