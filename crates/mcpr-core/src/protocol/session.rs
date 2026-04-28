use std::sync::Arc;

use chrono::{DateTime, Utc};
use dashmap::DashMap;

use crate::protocol::mcp::RequestId;

pub type SessionId = String;

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
    pub id: SessionId,
    pub state: SessionState,
    pub client_info: Option<ClientInfo>,
    pub created_at: DateTime<Utc>,
    pub last_active: DateTime<Utc>,
    pub request_count: u64,
    pub request_ids: Vec<RequestId>,
}

impl SessionInfo {
    pub fn new(id: SessionId) -> Self {
        let now = Utc::now();
        Self {
            id,
            state: SessionState::Created,
            client_info: None,
            created_at: now,
            last_active: now,
            request_count: 0,
            request_ids: Vec::new(),
        }
    }
}

/// In-memory session store backed by DashMap for lock-free concurrent access.
#[derive(Clone)]
pub struct SessionStore {
    sessions: Arc<DashMap<SessionId, SessionInfo>>,
    request_ids: Arc<DashMap<RequestId, SessionId>>,
}

impl SessionStore {
    pub fn new() -> Self {
        Self {
            sessions: Arc::new(DashMap::new()),
            request_ids: Arc::new(DashMap::new()),
        }
    }

    /// Insert a fresh session. Overwrites any existing entry with the same id.
    pub async fn new_session(&self, id: &str) -> SessionInfo {
        let info = SessionInfo::new(id.to_string());
        self.sessions.insert(id.to_string(), info.clone());
        info
    }

    /// Bind a request id to a session: appends to the session's request list,
    /// bumps `request_count` / `last_active`, and records the reverse index.
    /// No-op if the session doesn't exist.
    pub async fn attach_request(&self, request_id: RequestId, session_id: &str) {
        let Some(mut entry) = self.sessions.get_mut(session_id) else {
            return;
        };
        entry.last_active = Utc::now();
        entry.request_count += 1;
        entry.request_ids.push(request_id.clone());
        drop(entry);
        self.request_ids.insert(request_id, session_id.to_string());
    }

    /// Remove a session and clear any reverse-index entries that pointed at it.
    pub async fn end_session(&self, id: &str) {
        if let Some((_, info)) = self.sessions.remove(id) {
            for req_id in info.request_ids {
                self.request_ids.remove(&req_id);
            }
        }
    }

    /// All sessions, sorted by `last_active` (most recent first).
    pub async fn list_sessions(&self) -> Vec<SessionInfo> {
        let mut all: Vec<SessionInfo> = self.sessions.iter().map(|r| r.value().clone()).collect();
        all.sort_by(|a, b| b.last_active.cmp(&a.last_active));
        all
    }

    pub async fn get_session(&self, id: &str) -> Option<SessionInfo> {
        self.sessions.get(id).map(|r| r.clone())
    }

    pub async fn get_session_by_request(&self, request_id: &RequestId) -> Option<SessionInfo> {
        let sid = self.request_ids.get(request_id)?.clone();
        self.sessions.get(&sid).map(|r| r.clone())
    }
}

impl Default for SessionStore {
    fn default() -> Self {
        Self::new()
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

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;

    use std::thread::sleep;
    use std::time::Duration;

    use serde_json::json;

    fn req_num(n: i64) -> RequestId {
        RequestId::Number(n)
    }

    // ── SessionInfo::new ───────────────────────────────────────

    #[test]
    fn session_info_new__defaults() {
        let info = SessionInfo::new("s1".to_string());
        assert_eq!(info.id, "s1");
        assert_eq!(info.state, SessionState::Created);
        assert!(info.client_info.is_none());
        assert_eq!(info.request_count, 0);
        assert!(info.request_ids.is_empty());
        assert_eq!(info.created_at, info.last_active);
    }

    // ── new_session ────────────────────────────────────────────

    #[tokio::test]
    async fn new_session__inserts_and_returns() {
        let store = SessionStore::new();
        let info = store.new_session("abc").await;
        assert_eq!(info.id, "abc");
        assert_eq!(store.get_session("abc").await.unwrap().id, "abc");
    }

    #[tokio::test]
    async fn new_session__overwrites_existing() {
        let store = SessionStore::new();
        store.new_session("abc").await;
        store.attach_request(req_num(1), "abc").await;
        store.new_session("abc").await;
        let info = store.get_session("abc").await.unwrap();
        assert_eq!(info.request_count, 0);
        assert!(info.request_ids.is_empty());
    }

    // ── attach_request ─────────────────────────────────────────

    #[tokio::test]
    async fn attach_request__appends_and_bumps_count() {
        let store = SessionStore::new();
        store.new_session("s1").await;
        store.attach_request(req_num(1), "s1").await;
        store.attach_request(req_num(2), "s1").await;
        let info = store.get_session("s1").await.unwrap();
        assert_eq!(info.request_count, 2);
        assert_eq!(info.request_ids, vec![req_num(1), req_num(2)]);
    }

    #[tokio::test]
    async fn attach_request__writes_reverse_index() {
        let store = SessionStore::new();
        store.new_session("s1").await;
        store.attach_request(req_num(7), "s1").await;
        let found = store.get_session_by_request(&req_num(7)).await.unwrap();
        assert_eq!(found.id, "s1");
    }

    #[tokio::test]
    async fn attach_request__missing_session_is_noop() {
        let store = SessionStore::new();
        store.attach_request(req_num(1), "missing").await;
        assert!(store.get_session("missing").await.is_none());
        assert!(store.get_session_by_request(&req_num(1)).await.is_none());
    }

    #[tokio::test]
    async fn attach_request__updates_last_active() {
        let store = SessionStore::new();
        let created = store.new_session("s1").await;
        sleep(Duration::from_millis(2));
        store.attach_request(req_num(1), "s1").await;
        let after = store.get_session("s1").await.unwrap();
        assert!(after.last_active > created.last_active);
    }

    // ── end_session ────────────────────────────────────────────

    #[tokio::test]
    async fn end_session__removes_session_and_reverse_entries() {
        let store = SessionStore::new();
        store.new_session("s1").await;
        store.attach_request(req_num(1), "s1").await;
        store.attach_request(req_num(2), "s1").await;

        store.end_session("s1").await;

        assert!(store.get_session("s1").await.is_none());
        assert!(store.get_session_by_request(&req_num(1)).await.is_none());
        assert!(store.get_session_by_request(&req_num(2)).await.is_none());
    }

    #[tokio::test]
    async fn end_session__leaves_other_sessions_intact() {
        let store = SessionStore::new();
        store.new_session("s1").await;
        store.new_session("s2").await;
        store.attach_request(req_num(1), "s1").await;
        store.attach_request(req_num(2), "s2").await;

        store.end_session("s1").await;

        assert!(store.get_session("s2").await.is_some());
        assert_eq!(
            store.get_session_by_request(&req_num(2)).await.unwrap().id,
            "s2"
        );
    }

    #[tokio::test]
    async fn end_session__missing_id_is_noop() {
        let store = SessionStore::new();
        store.end_session("nope").await;
        assert!(store.list_sessions().await.is_empty());
    }

    // ── list_sessions ──────────────────────────────────────────

    #[tokio::test]
    async fn list_sessions__empty() {
        let store = SessionStore::new();
        assert!(store.list_sessions().await.is_empty());
    }

    #[tokio::test]
    async fn list_sessions__sorted_by_last_active_desc() {
        let store = SessionStore::new();
        store.new_session("oldest").await;
        sleep(Duration::from_millis(2));
        store.new_session("middle").await;
        sleep(Duration::from_millis(2));
        store.new_session("newest").await;

        let ids: Vec<_> = store
            .list_sessions()
            .await
            .into_iter()
            .map(|s| s.id)
            .collect();
        assert_eq!(ids, vec!["newest", "middle", "oldest"]);
    }

    #[tokio::test]
    async fn list_sessions__attach_request_promotes_session() {
        let store = SessionStore::new();
        store.new_session("a").await;
        sleep(Duration::from_millis(2));
        store.new_session("b").await;
        sleep(Duration::from_millis(2));
        store.attach_request(req_num(1), "a").await;

        let ids: Vec<_> = store
            .list_sessions()
            .await
            .into_iter()
            .map(|s| s.id)
            .collect();
        assert_eq!(ids, vec!["a", "b"]);
    }

    // ── get_session ────────────────────────────────────────────

    #[tokio::test]
    async fn get_session__missing_is_none() {
        let store = SessionStore::new();
        assert!(store.get_session("nope").await.is_none());
    }

    // ── get_session_by_request ─────────────────────────────────

    #[tokio::test]
    async fn get_session_by_request__missing_is_none() {
        let store = SessionStore::new();
        assert!(store.get_session_by_request(&req_num(99)).await.is_none());
    }

    #[tokio::test]
    async fn get_session_by_request__string_request_id() {
        let store = SessionStore::new();
        store.new_session("s1").await;
        let rid = RequestId::String("req-abc".into());
        store.attach_request(rid.clone(), "s1").await;
        assert_eq!(store.get_session_by_request(&rid).await.unwrap().id, "s1");
    }

    // ── parse_client_info ──────────────────────────────────────

    #[test]
    fn parse_client_info__name_and_version() {
        let info = parse_client_info(&json!({
            "clientInfo": {"name": "Claude Code", "version": "1.2.0"}
        }))
        .unwrap();
        assert_eq!(info.name, "Claude Code");
        assert_eq!(info.version.as_deref(), Some("1.2.0"));
    }

    #[test]
    fn parse_client_info__name_only() {
        let info = parse_client_info(&json!({
            "clientInfo": {"name": "cursor"}
        }))
        .unwrap();
        assert_eq!(info.name, "cursor");
        assert!(info.version.is_none());
    }

    #[test]
    fn parse_client_info__missing_clientinfo_is_none() {
        assert!(parse_client_info(&json!({"capabilities": {}})).is_none());
    }

    #[test]
    fn parse_client_info__missing_name_is_none() {
        assert!(parse_client_info(&json!({"clientInfo": {"version": "1.0"}})).is_none());
    }

    #[test]
    fn parse_client_info__non_string_name_is_none() {
        assert!(parse_client_info(&json!({"clientInfo": {"name": 42}})).is_none());
    }
}
