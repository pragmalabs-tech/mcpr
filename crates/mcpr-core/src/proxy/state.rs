//! Proxy runtime state — shared across the proxy process.
//!
//! Tracks the health and status of all proxy connections: MCP upstream,
//! tunnel, widgets, cloud sync, and request counters. This is the single
//! source of truth for "what is the proxy doing right now?"
//!
//! Used by:
//! - `mcpr start` — populates state during operation.
//! - `mcpr status` — could read state via admin API (future).
//! - `mcpr proxy view` — TUI viewer will read this state (future).
//! - Health check loop — updates MCP/widget/tunnel status periodically.

use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Connection status for an upstream service (MCP, tunnel, widgets).
#[derive(Clone, Copy, PartialEq)]
pub enum ConnectionStatus {
    /// Status not yet determined (initial state).
    Unknown,
    /// Service is unreachable or down.
    Disconnected,
    /// Connection attempt in progress (tunnel only).
    Connecting,
    /// Service is healthy and responding.
    Connected,
    /// Tunnel was forcibly disconnected by the relay (e.g., subdomain conflict).
    Evicted,
    /// Server is reachable but does not speak MCP protocol.
    NotMcp,
}

impl ConnectionStatus {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Unknown => "Unknown",
            Self::Disconnected => "Disconnected",
            Self::Connecting => "Connecting…",
            Self::Connected => "Connected",
            Self::Evicted => "Evicted",
            Self::NotMcp => "Not MCP",
        }
    }
}

/// Cloud sync status from the last flush attempt.
pub enum CloudSyncStatus {
    Ok { count: usize },
    Failed { message: String },
}

/// Runtime state of a running proxy instance.
///
/// All fields are updated by background tasks (health checks, request handlers)
/// and read by status/admin endpoints. Protected by a Mutex for simplicity —
/// contention is negligible since updates are infrequent (every few seconds).
pub struct ProxyState {
    /// Public URL where AI clients connect (e.g., http://localhost:3000 or tunnel URL).
    pub proxy_url: String,
    /// Tunnel public URL (empty if tunnel disabled).
    pub tunnel_url: String,
    /// Upstream MCP server URL from config.
    pub mcp_upstream: String,
    /// Widget source description ("URL", "path", or "(none)").
    pub widgets: String,

    /// MCP upstream connection health.
    pub mcp_status: ConnectionStatus,
    /// Optional warning about MCP upstream (e.g., "Server requires auth").
    pub mcp_warning: Option<String>,
    /// Tunnel connection health.
    pub tunnel_status: ConnectionStatus,
    /// Widget source connection health.
    pub widgets_status: ConnectionStatus,
    /// Number of discovered widgets.
    pub widget_count: Option<usize>,
    /// Names of discovered widgets.
    pub widget_names: Vec<String>,

    /// Cloud sync endpoint URL (None if cloud not configured).
    pub cloud_endpoint: Option<String>,
    /// Last cloud sync status.
    pub cloud_sync: Option<CloudSyncStatus>,

    /// When this proxy instance started.
    pub started_at: Instant,
    /// Total number of requests handled.
    pub request_count: u64,
}

impl ProxyState {
    pub fn new() -> Self {
        Self {
            proxy_url: String::new(),
            tunnel_url: String::new(),
            mcp_upstream: String::new(),
            widgets: "(none)".into(),
            mcp_status: ConnectionStatus::Unknown,
            mcp_warning: None,
            tunnel_status: ConnectionStatus::Disconnected,
            widgets_status: ConnectionStatus::Unknown,
            widget_count: None,
            widget_names: Vec::new(),
            cloud_endpoint: None,
            cloud_sync: None,
            started_at: Instant::now(),
            request_count: 0,
        }
    }

    /// Mark the MCP upstream as confirmed connected (clear any warning).
    pub fn confirm_mcp_connected(&mut self) {
        self.mcp_status = ConnectionStatus::Connected;
        self.mcp_warning = None;
    }

    /// Increment request counter.
    pub fn record_request(&mut self) {
        self.request_count += 1;
    }

    /// Human-readable uptime string.
    pub fn uptime(&self) -> String {
        let secs = self.started_at.elapsed().as_secs();
        if secs < 60 {
            format!("{secs}s")
        } else if secs < 3600 {
            format!("{}m {}s", secs / 60, secs % 60)
        } else {
            format!("{}h {}m", secs / 3600, (secs % 3600) / 60)
        }
    }
}

impl Default for ProxyState {
    fn default() -> Self {
        Self::new()
    }
}

/// Thread-safe shared proxy state.
///
/// Use [`lock_state`] instead of `.lock().unwrap()` to handle mutex
/// poisoning gracefully (recovers instead of panicking).
pub type SharedProxyState = Arc<Mutex<ProxyState>>;

/// Create a new shared proxy state.
pub fn new_shared_state() -> SharedProxyState {
    Arc::new(Mutex::new(ProxyState::new()))
}

/// Lock the shared state, recovering from poison if a thread panicked.
///
/// Prefer this over `.lock().unwrap()` — it never panics.
pub fn lock_state(state: &SharedProxyState) -> std::sync::MutexGuard<'_, ProxyState> {
    state.lock().unwrap_or_else(|e| e.into_inner())
}
