use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};

use crate::protocol::mcp::{ClientInfo, RequestId};

pub type SessionId = String;

/// Observed MCP session state — inferred from method calls passing through the proxy.
/// The proxy doesn't control transitions; it observes them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionState {
    Active,
    Closed,
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
    pub fn new(id: SessionId, client_info: Option<ClientInfo>, request_id: RequestId) -> Self {
        let now = Utc::now();
        Self {
            id,
            state: SessionState::Active,
            client_info,
            created_at: now,
            last_active: now,
            request_count: 1,
            request_ids: vec![request_id],
        }
    }

    pub fn merge(&mut self, other: Self) {
        if self.id != other.id {
            return;
        }

        self.last_active = other.last_active;
        self.request_count += other.request_count;
        self.request_ids.extend(other.request_ids);
    }
}

/// In-memory session store. Both indexes live behind one `Mutex` so every
/// operation is atomic across `sessions` and `request_ids`.
#[derive(Clone)]
pub struct ActiveSessionStore {
    inner: Arc<Mutex<Inner>>,
}

struct Inner {
    sessions: HashMap<SessionId, SessionInfo>,
    request_ids: HashMap<RequestId, SessionId>,
}

impl ActiveSessionStore {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                sessions: HashMap::new(),
                request_ids: HashMap::new(),
            })),
        }
    }

    /// Add a request to session store.
    /// Can be used to create a new session if one doesn't exist.
    ///
    /// Returns `Some(info)` when state changed (new session or new request) so
    /// callers can emit. Returns `None` when the request was already tracked.
    pub async fn track_request(
        &self,
        session_id: SessionId,
        request_id: RequestId,
        client_info: Option<ClientInfo>,
    ) -> Option<SessionInfo> {
        let mut inner = self.inner.lock().unwrap();

        if inner.request_ids.contains_key(&request_id) {
            return None;
        }

        let info = match inner.sessions.get_mut(&session_id) {
            Some(existing) => {
                existing.last_active = Utc::now();
                existing.request_count += 1;
                existing.request_ids.push(request_id.clone());
                existing.clone()
            }
            None => {
                let new_info =
                    SessionInfo::new(session_id.clone(), client_info, request_id.clone());
                inner.sessions.insert(session_id.clone(), new_info.clone());
                new_info
            }
        };

        inner.request_ids.insert(request_id, session_id);
        Some(info)
    }

    /// Remove a session and clear any reverse-index entries that pointed at it.
    pub async fn end_session(&self, id: &str) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(info) = inner.sessions.remove(id) {
            for req_id in &info.request_ids {
                inner.request_ids.remove(req_id);
            }
        }
    }

    /// All sessions, sorted by `last_active` (most recent first).
    pub async fn list_sessions(&self) -> Vec<SessionInfo> {
        let inner = self.inner.lock().unwrap();
        let mut all: Vec<SessionInfo> = inner.sessions.values().cloned().collect();
        all.sort_by(|a, b| b.last_active.cmp(&a.last_active));
        all
    }

    pub async fn get_session(&self, id: &str) -> Option<SessionInfo> {
        self.inner.lock().unwrap().sessions.get(id).cloned()
    }

    pub async fn get_session_by_request(&self, request_id: &RequestId) -> Option<SessionInfo> {
        let inner = self.inner.lock().unwrap();
        let sid = inner.request_ids.get(request_id)?;
        inner.sessions.get(sid).cloned()
    }
}

impl Default for ActiveSessionStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;

    use std::thread::sleep;
    use std::time::Duration;

    fn req_num(n: i64) -> RequestId {
        RequestId::Number(n)
    }

    fn client(name: &str) -> ClientInfo {
        ClientInfo {
            name: name.to_string(),
            version: None,
        }
    }

    // ── SessionInfo::new ───────────────────────────────────────

    #[test]
    fn session_info_new__seeds_first_request() {
        let info = SessionInfo::new("s1".to_string(), None, req_num(1));
        assert_eq!(info.id, "s1");
        assert_eq!(info.state, SessionState::Active);
        assert!(info.client_info.is_none());
        assert_eq!(info.request_count, 1);
        assert_eq!(info.request_ids, vec![req_num(1)]);
        assert_eq!(info.created_at, info.last_active);
    }

    #[test]
    fn session_info_new__captures_client_info() {
        let info = SessionInfo::new("s1".to_string(), Some(client("cursor")), req_num(1));
        assert_eq!(info.client_info.as_ref().unwrap().name, "cursor");
    }

    // ── SessionInfo::merge ─────────────────────────────────────

    #[test]
    fn session_info_merge__appends_requests_and_count() {
        let mut a = SessionInfo::new("s1".to_string(), None, req_num(1));
        let b = SessionInfo::new("s1".to_string(), None, req_num(2));
        a.merge(b);
        assert_eq!(a.request_count, 2);
        assert_eq!(a.request_ids, vec![req_num(1), req_num(2)]);
    }

    #[test]
    fn session_info_merge__ignores_mismatched_id() {
        let mut a = SessionInfo::new("s1".to_string(), None, req_num(1));
        let b = SessionInfo::new("s2".to_string(), None, req_num(2));
        a.merge(b);
        assert_eq!(a.request_count, 1);
        assert_eq!(a.request_ids, vec![req_num(1)]);
    }

    // ── track_request ──────────────────────────────────────────

    #[tokio::test]
    async fn track_request__creates_session_when_missing() {
        let store = ActiveSessionStore::new();
        let info = store
            .track_request("s1".into(), req_num(1), Some(client("cursor")))
            .await
            .unwrap();
        assert_eq!(info.id, "s1");
        assert_eq!(info.request_count, 1);
        assert_eq!(info.request_ids, vec![req_num(1)]);
        assert_eq!(info.client_info.as_ref().unwrap().name, "cursor");
    }

    #[tokio::test]
    async fn track_request__appends_to_existing_session() {
        let store = ActiveSessionStore::new();
        store.track_request("s1".into(), req_num(1), None).await;
        let info = store
            .track_request("s1".into(), req_num(2), None)
            .await
            .unwrap();
        assert_eq!(info.request_count, 2);
        assert_eq!(info.request_ids, vec![req_num(1), req_num(2)]);
    }

    #[tokio::test]
    async fn track_request__duplicate_request_returns_none() {
        let store = ActiveSessionStore::new();
        store.track_request("s1".into(), req_num(1), None).await;
        let again = store.track_request("s1".into(), req_num(1), None).await;
        assert!(again.is_none());
        let info = store.get_session("s1").await.unwrap();
        assert_eq!(info.request_count, 1);
    }

    #[tokio::test]
    async fn track_request__duplicate_request_across_sessions_returns_none() {
        let store = ActiveSessionStore::new();
        store.track_request("s1".into(), req_num(1), None).await;
        let again = store.track_request("s2".into(), req_num(1), None).await;
        assert!(again.is_none());
        assert!(store.get_session("s2").await.is_none());
    }

    #[tokio::test]
    async fn track_request__writes_reverse_index() {
        let store = ActiveSessionStore::new();
        store.track_request("s1".into(), req_num(7), None).await;
        let found = store.get_session_by_request(&req_num(7)).await.unwrap();
        assert_eq!(found.id, "s1");
    }

    #[tokio::test]
    async fn track_request__updates_last_active() {
        let store = ActiveSessionStore::new();
        let created = store
            .track_request("s1".into(), req_num(1), None)
            .await
            .unwrap();
        sleep(Duration::from_millis(2));
        let after = store
            .track_request("s1".into(), req_num(2), None)
            .await
            .unwrap();
        assert!(after.last_active > created.last_active);
        assert_eq!(after.created_at, created.created_at);
    }

    #[tokio::test]
    async fn track_request__keeps_initial_client_info_on_append() {
        let store = ActiveSessionStore::new();
        store
            .track_request("s1".into(), req_num(1), Some(client("cursor")))
            .await;
        store
            .track_request("s1".into(), req_num(2), Some(client("other")))
            .await;
        let info = store.get_session("s1").await.unwrap();
        assert_eq!(info.client_info.as_ref().unwrap().name, "cursor");
    }

    // ── end_session ────────────────────────────────────────────

    #[tokio::test]
    async fn end_session__removes_session_and_reverse_entries() {
        let store = ActiveSessionStore::new();
        store.track_request("s1".into(), req_num(1), None).await;
        store.track_request("s1".into(), req_num(2), None).await;

        store.end_session("s1").await;

        assert!(store.get_session("s1").await.is_none());
        assert!(store.get_session_by_request(&req_num(1)).await.is_none());
        assert!(store.get_session_by_request(&req_num(2)).await.is_none());
    }

    #[tokio::test]
    async fn end_session__leaves_other_sessions_intact() {
        let store = ActiveSessionStore::new();
        store.track_request("s1".into(), req_num(1), None).await;
        store.track_request("s2".into(), req_num(2), None).await;

        store.end_session("s1").await;

        assert!(store.get_session("s2").await.is_some());
        assert_eq!(
            store.get_session_by_request(&req_num(2)).await.unwrap().id,
            "s2"
        );
    }

    #[tokio::test]
    async fn end_session__missing_id_is_noop() {
        let store = ActiveSessionStore::new();
        store.end_session("nope").await;
        assert!(store.list_sessions().await.is_empty());
    }

    // ── list_sessions ──────────────────────────────────────────

    #[tokio::test]
    async fn list_sessions__empty() {
        let store = ActiveSessionStore::new();
        assert!(store.list_sessions().await.is_empty());
    }

    #[tokio::test]
    async fn list_sessions__sorted_by_last_active_desc() {
        let store = ActiveSessionStore::new();
        store.track_request("oldest".into(), req_num(1), None).await;
        sleep(Duration::from_millis(2));
        store.track_request("middle".into(), req_num(2), None).await;
        sleep(Duration::from_millis(2));
        store.track_request("newest".into(), req_num(3), None).await;

        let ids: Vec<_> = store
            .list_sessions()
            .await
            .into_iter()
            .map(|s| s.id)
            .collect();
        assert_eq!(ids, vec!["newest", "middle", "oldest"]);
    }

    #[tokio::test]
    async fn list_sessions__track_request_promotes_session() {
        let store = ActiveSessionStore::new();
        store.track_request("a".into(), req_num(1), None).await;
        sleep(Duration::from_millis(2));
        store.track_request("b".into(), req_num(2), None).await;
        sleep(Duration::from_millis(2));
        store.track_request("a".into(), req_num(3), None).await;

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
        let store = ActiveSessionStore::new();
        assert!(store.get_session("nope").await.is_none());
    }

    // ── get_session_by_request ─────────────────────────────────

    #[tokio::test]
    async fn get_session_by_request__missing_is_none() {
        let store = ActiveSessionStore::new();
        assert!(store.get_session_by_request(&req_num(99)).await.is_none());
    }

    #[tokio::test]
    async fn get_session_by_request__string_request_id() {
        let store = ActiveSessionStore::new();
        let rid = RequestId::String("req-abc".into());
        store.track_request("s1".into(), rid.clone(), None).await;
        assert_eq!(store.get_session_by_request(&rid).await.unwrap().id, "s1");
    }
}
