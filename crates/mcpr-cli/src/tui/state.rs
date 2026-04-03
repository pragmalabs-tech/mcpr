use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Instant;

pub use crate::logger::LogEntry;

const MAX_LOG_ENTRIES: usize = 10_000;

#[derive(Clone, Copy, PartialEq)]
pub enum Tab {
    Requests,
    Sessions,
}

#[derive(Clone, Copy, PartialEq)]
pub enum ConnectionStatus {
    Unknown,
    Disconnected,
    Connecting,
    Connected,
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

pub struct TuiState {
    // Info panel
    pub proxy_url: String,
    pub tunnel_url: String,
    pub mcp_upstream: String,
    pub widgets: String,
    pub tunnel_status: ConnectionStatus,
    /// Whether the tunnel is using an anonymous (temporary) subdomain.
    pub tunnel_anonymous: bool,
    pub mcp_status: ConnectionStatus,
    pub widgets_status: ConnectionStatus,
    pub widget_count: Option<usize>,
    pub widget_names: Vec<String>,
    pub mcp_warning: Option<String>,
    pub started_at: Instant,
    pub request_count: u64,

    // Right panel
    pub active_tab: Tab,
    pub log_entries: VecDeque<LogEntry>,
    pub auto_scroll: bool,
    pub scroll_offset: u16,

    // Sessions tab
    pub selected_session: usize,
    pub session_detail_scroll: u16,
}

impl TuiState {
    pub fn new() -> Self {
        Self {
            proxy_url: String::new(),
            tunnel_url: String::new(),
            mcp_upstream: String::new(),
            widgets: "(none)".into(),
            tunnel_status: ConnectionStatus::Disconnected,
            tunnel_anonymous: false,
            mcp_status: ConnectionStatus::Unknown,
            widgets_status: ConnectionStatus::Unknown,
            widget_count: None,
            widget_names: Vec::new(),
            mcp_warning: None,
            started_at: Instant::now(),
            request_count: 0,
            active_tab: Tab::Requests,
            log_entries: VecDeque::new(),
            auto_scroll: true,
            scroll_offset: 0,
            selected_session: 0,
            session_detail_scroll: 0,
        }
    }

    pub fn push_log(&mut self, entry: LogEntry) {
        self.request_count += 1;
        self.log_entries.push_back(entry);
        if self.log_entries.len() > MAX_LOG_ENTRIES {
            self.log_entries.pop_front();
        }
    }

    /// Mark the MCP upstream as confirmed connected (no warning).
    pub fn confirm_mcp_connected(&mut self) {
        self.mcp_status = ConnectionStatus::Connected;
        self.mcp_warning = None;
    }

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

pub type SharedTuiState = Arc<Mutex<TuiState>>;

pub fn new_shared_state() -> SharedTuiState {
    Arc::new(Mutex::new(TuiState::new()))
}
