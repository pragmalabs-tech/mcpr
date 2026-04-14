use std::future::Future;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use dashmap::DashMap;

/// Observed MCP session state — inferred from method calls passing through the proxy.
/// The proxy doesn't control transitions; it observes them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionState {
    Created,
    Initialized,
    Active,
    Closed,
}

/// Client identity extracted from the MCP `initialize` request's `clientInfo` param.
#[derive(Debug, Clone)]
pub struct ClientInfo {
    pub name: String,
    pub version: Option<String>,
}

/// Observed session metadata tracked by the proxy for debugging/observability.
#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub id: String,
    pub state: SessionState,
    pub client_info: Option<ClientInfo>,
    pub created_at: DateTime<Utc>,
    pub last_active: DateTime<Utc>,
    pub request_count: u64,
}

impl SessionInfo {
    pub fn new(id: String) -> Self {
        let now = Utc::now();
        Self {
            id,
            state: SessionState::Created,
            client_info: None,
            created_at: now,
            last_active: now,
            request_count: 0,
        }
    }
}

/// Trait for session storage backends.
/// Async to support I/O-backed stores (Redis, database, logging).
pub trait SessionStore: Send + Sync + 'static {
    fn create(&self, id: &str) -> impl Future<Output = SessionInfo> + Send;
    fn get(&self, id: &str) -> impl Future<Output = Option<SessionInfo>> + Send;
    fn touch(&self, id: &str) -> impl Future<Output = ()> + Send;
    fn update_state(&self, id: &str, state: SessionState) -> impl Future<Output = ()> + Send;
    fn set_client_info(&self, id: &str, info: ClientInfo) -> impl Future<Output = ()> + Send;
    fn remove(&self, id: &str) -> impl Future<Output = ()> + Send;
    fn list(&self) -> impl Future<Output = Vec<SessionInfo>> + Send;
}

/// In-memory session store backed by DashMap for lock-free concurrent access.
#[derive(Clone)]
pub struct MemorySessionStore {
    sessions: Arc<DashMap<String, SessionInfo>>,
}

impl MemorySessionStore {
    pub fn new() -> Self {
        Self {
            sessions: Arc::new(DashMap::new()),
        }
    }

    /// Sync access to session list — for use in non-async contexts (TUI rendering).
    pub fn list_sync(&self) -> Vec<SessionInfo> {
        self.sessions.iter().map(|r| r.value().clone()).collect()
    }
}

impl Default for MemorySessionStore {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionStore for MemorySessionStore {
    async fn create(&self, id: &str) -> SessionInfo {
        let info = SessionInfo::new(id.to_string());
        self.sessions.insert(id.to_string(), info.clone());
        info
    }

    async fn get(&self, id: &str) -> Option<SessionInfo> {
        self.sessions.get(id).map(|r| r.clone())
    }

    async fn touch(&self, id: &str) {
        if let Some(mut entry) = self.sessions.get_mut(id) {
            entry.last_active = Utc::now();
            entry.request_count += 1;
        }
    }

    async fn update_state(&self, id: &str, state: SessionState) {
        if let Some(mut entry) = self.sessions.get_mut(id) {
            entry.state = state;
            entry.last_active = Utc::now();
        }
    }

    async fn set_client_info(&self, id: &str, info: ClientInfo) {
        if let Some(mut entry) = self.sessions.get_mut(id) {
            entry.client_info = Some(info);
        }
    }

    async fn remove(&self, id: &str) {
        self.sessions.remove(id);
    }

    async fn list(&self) -> Vec<SessionInfo> {
        self.sessions.iter().map(|r| r.value().clone()).collect()
    }
}

/// Extract `clientInfo` from MCP initialize request params.
pub fn parse_client_info(params: &serde_json::Value) -> Option<ClientInfo> {
    let client_info = params.get("clientInfo")?;
    let name = client_info.get("name")?.as_str()?.to_string();
    let version = client_info
        .get("version")
        .and_then(|v| v.as_str())
        .map(String::from);
    Some(ClientInfo { name, version })
}
