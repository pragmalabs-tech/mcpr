//! [`ProxyState`] — everything one running proxy instance needs to serve
//! traffic: upstream client, rewrite config, sessions, schema manager,
//! per-proxy health, event bus handle. Request handlers take
//! `Arc<ProxyState>`.

use std::sync::Arc;

use arc_swap::ArcSwap;

use crate::event::EventBus;
use crate::protocol::schema_manager::{MemorySchemaStore, SchemaManager};
use crate::protocol::session::MemorySessionStore;

use super::RewriteConfig;
use super::forwarding::UpstreamClient;
use super::health::SharedProxyHealth;

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
    /// Lock-free: readers call `.load()` (sync, ~5 ns); a writer
    /// wanting to swap config does `.store(Arc::new(new))`.
    pub rewrite_config: Arc<ArcSwap<RewriteConfig>>,

    // ── runtime tracking ──
    pub sessions: MemorySessionStore,
    pub schema_manager: Arc<SchemaManager<MemorySchemaStore>>,

    // ── per-proxy health display + tunnel callbacks ──
    pub health: SharedProxyHealth,

    // ── observability (cloned handle, cheap to clone) ──
    pub event_bus: EventBus,
}
