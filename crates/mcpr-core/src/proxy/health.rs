//! Per-proxy connection health and display state.
//!
//! Tracks what one proxy instance is currently doing — MCP upstream health,
//! tunnel connection status, cloud sync, request counters. Updated by
//! background tasks (health checks, request handlers, tunnel callbacks) and
//! read by the admin API, TUI, and status commands.
//!
//! Lives behind an `Arc<Mutex<_>>` because updates come from many callers and
//! readers pull snapshots; contention is negligible (updates are infrequent).

use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Connection status for an upstream service (MCP, tunnel).
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

/// Display + health state for one proxy instance.
pub struct ProxyHealth {
    /// Public URL where AI clients connect (e.g., http://localhost:3000 or tunnel URL).
    pub proxy_url: String,
    /// Tunnel public URL (empty if tunnel disabled).
    pub tunnel_url: String,
    /// Upstream MCP server URL from config.
    pub mcp_upstream: String,

    /// MCP upstream connection health.
    pub mcp_status: ConnectionStatus,
    /// Optional warning about MCP upstream (e.g., "Server requires auth").
    pub mcp_warning: Option<String>,
    /// Tunnel connection health.
    pub tunnel_status: ConnectionStatus,

    /// Cloud sync endpoint URL (None if cloud not configured).
    pub cloud_endpoint: Option<String>,
    /// Last cloud sync status.
    pub cloud_sync: Option<CloudSyncStatus>,

    /// When this proxy instance started.
    pub started_at: Instant,
    /// Total number of requests handled.
    pub request_count: u64,
}

impl ProxyHealth {
    pub fn new() -> Self {
        Self {
            proxy_url: String::new(),
            tunnel_url: String::new(),
            mcp_upstream: String::new(),
            mcp_status: ConnectionStatus::Unknown,
            mcp_warning: None,
            tunnel_status: ConnectionStatus::Disconnected,
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

impl Default for ProxyHealth {
    fn default() -> Self {
        Self::new()
    }
}

/// Thread-safe shared proxy health. Prefer [`lock_health`] over `.lock().unwrap()`
/// to handle poisoning without panic.
pub type SharedProxyHealth = Arc<Mutex<ProxyHealth>>;

/// Create a new shared proxy health container.
pub fn new_shared_health() -> SharedProxyHealth {
    Arc::new(Mutex::new(ProxyHealth::new()))
}

/// Lock the shared health, recovering from poison if a thread panicked.
pub fn lock_health(health: &SharedProxyHealth) -> std::sync::MutexGuard<'_, ProxyHealth> {
    health.lock().unwrap_or_else(|e| e.into_inner())
}
