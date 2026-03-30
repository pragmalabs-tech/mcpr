use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Instant;

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

pub struct LogEntry {
    pub timestamp: String,
    pub method: String,
    pub path: String,
    pub mcp_method: Option<String>,
    pub session_id: Option<String>,
    pub status: u16,
    pub note: String,
    pub upstream_url: Option<String>,
    pub resp_size: Option<usize>,
    pub duration_ms: Option<u64>,
    /// Time spent waiting for upstream (network). Proxy overhead = duration_ms - upstream_ms.
    pub upstream_ms: Option<u64>,
    /// JSON-RPC error code from the response body (if the response is a JSON-RPC error).
    pub jsonrpc_error: Option<(i64, String)>,
    /// Extra detail: tool name for tools/call, resource URI for resources/read, etc.
    pub detail: Option<String>,
}

impl LogEntry {
    pub fn new(method: &str, path: &str, status: u16, note: &str) -> Self {
        Self {
            timestamp: chrono::Local::now().format("%H:%M:%S").to_string(),
            method: method.to_string(),
            path: path.to_string(),
            mcp_method: None,
            session_id: None,
            status,
            note: note.to_string(),
            upstream_url: None,
            resp_size: None,
            duration_ms: None,
            upstream_ms: None,
            jsonrpc_error: None,
            detail: None,
        }
    }

    pub fn session_id(mut self, id: &str) -> Self {
        self.session_id = Some(id.to_string());
        self
    }

    pub fn maybe_session_id(mut self, id: Option<&str>) -> Self {
        self.session_id = id.map(String::from);
        self
    }

    pub fn mcp_method(mut self, m: &str) -> Self {
        self.mcp_method = Some(m.to_string());
        self
    }

    pub fn upstream(mut self, url: &str) -> Self {
        self.upstream_url = Some(url.to_string());
        self
    }

    pub fn size(mut self, bytes: usize) -> Self {
        self.resp_size = Some(bytes);
        self
    }

    pub fn duration(mut self, start: Instant) -> Self {
        self.duration_ms = Some(start.elapsed().as_millis() as u64);
        self
    }

    pub fn upstream_duration(mut self, ms: u64) -> Self {
        self.upstream_ms = Some(ms);
        self
    }

    pub fn jsonrpc_error(mut self, code: i64, message: &str) -> Self {
        self.jsonrpc_error = Some((code, message.to_string()));
        self
    }

    pub fn detail(mut self, d: &str) -> Self {
        self.detail = Some(d.to_string());
        self
    }

    pub fn maybe_detail(mut self, d: Option<&str>) -> Self {
        self.detail = d.map(String::from);
        self
    }
}

pub struct TuiState {
    // Info panel
    pub proxy_url: String,
    pub tunnel_url: String,
    pub mcp_upstream: String,
    pub widgets: String,
    pub tunnel_status: ConnectionStatus,
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
