use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::http::HeaderMap;
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
///
/// TODO: idle-session cleanup. Today sessions only end on an explicit
/// client `DELETE` (see `end_session`). If the client crashes or never
/// sends `DELETE`, the entry lives forever — `last_active` keeps
/// advancing only while requests flow, so a TTL-based sweeper that
/// closes sessions inactive for N minutes is the missing half.
#[derive(Clone)]
pub struct SessionStore {
    inner: Arc<Mutex<Inner>>,
}

struct Inner {
    sessions: HashMap<SessionId, SessionInfo>,
    request_ids: HashMap<RequestId, SessionId>,
}

impl SessionStore {
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
    pub fn track_request(
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
    /// Returns the removed `SessionInfo` (with `state` flipped to `Closed`) so
    /// callers can emit a final event. `None` if the session was unknown.
    pub fn end_session(&self, id: &str) -> Option<SessionInfo> {
        let mut inner = self.inner.lock().unwrap();
        let mut info = inner.sessions.remove(id)?;
        for req_id in &info.request_ids {
            inner.request_ids.remove(req_id);
        }
        info.state = SessionState::Closed;
        Some(info)
    }

    /// All sessions, sorted by `last_active` (most recent first).
    pub fn list_sessions(&self) -> Vec<SessionInfo> {
        let inner = self.inner.lock().unwrap();
        let mut all: Vec<SessionInfo> = inner.sessions.values().cloned().collect();
        all.sort_by(|a, b| b.last_active.cmp(&a.last_active));
        all
    }

    pub fn get_session(&self, id: &str) -> Option<SessionInfo> {
        self.inner.lock().unwrap().sessions.get(id).cloned()
    }

    pub fn get_session_by_request(&self, request_id: &RequestId) -> Option<SessionInfo> {
        let inner = self.inner.lock().unwrap();
        let sid = inner.request_ids.get(request_id)?;
        inner.sessions.get(sid).cloned()
    }
}

impl Default for SessionStore {
    fn default() -> Self {
        Self::new()
    }
}

/// Extract the session id from the inbound `mcp-session-id` header.
/// Returns `None` if the header is absent or not valid UTF-8.
pub fn session_id_from_headers(headers: &HeaderMap) -> Option<SessionId> {
    headers
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .map(String::from)
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

    #[test]
    fn track_request__creates_session_when_missing() {
        let store = SessionStore::new();
        let info = store
            .track_request("s1".into(), req_num(1), Some(client("cursor")))
            .unwrap();
        assert_eq!(info.id, "s1");
        assert_eq!(info.request_count, 1);
        assert_eq!(info.request_ids, vec![req_num(1)]);
        assert_eq!(info.client_info.as_ref().unwrap().name, "cursor");
    }

    #[test]
    fn track_request__appends_to_existing_session() {
        let store = SessionStore::new();
        store.track_request("s1".into(), req_num(1), None);
        let info = store.track_request("s1".into(), req_num(2), None).unwrap();
        assert_eq!(info.request_count, 2);
        assert_eq!(info.request_ids, vec![req_num(1), req_num(2)]);
    }

    #[test]
    fn track_request__duplicate_request_returns_none() {
        let store = SessionStore::new();
        store.track_request("s1".into(), req_num(1), None);
        let again = store.track_request("s1".into(), req_num(1), None);
        assert!(again.is_none());
        let info = store.get_session("s1").unwrap();
        assert_eq!(info.request_count, 1);
    }

    #[test]
    fn track_request__duplicate_request_across_sessions_returns_none() {
        let store = SessionStore::new();
        store.track_request("s1".into(), req_num(1), None);
        let again = store.track_request("s2".into(), req_num(1), None);
        assert!(again.is_none());
        assert!(store.get_session("s2").is_none());
    }

    #[test]
    fn track_request__writes_reverse_index() {
        let store = SessionStore::new();
        store.track_request("s1".into(), req_num(7), None);
        let found = store.get_session_by_request(&req_num(7)).unwrap();
        assert_eq!(found.id, "s1");
    }

    #[test]
    fn track_request__updates_last_active() {
        let store = SessionStore::new();
        let created = store.track_request("s1".into(), req_num(1), None).unwrap();
        sleep(Duration::from_millis(2));
        let after = store.track_request("s1".into(), req_num(2), None).unwrap();
        assert!(after.last_active > created.last_active);
        assert_eq!(after.created_at, created.created_at);
    }

    #[test]
    fn track_request__keeps_initial_client_info_on_append() {
        let store = SessionStore::new();
        store.track_request("s1".into(), req_num(1), Some(client("cursor")));
        store.track_request("s1".into(), req_num(2), Some(client("other")));
        let info = store.get_session("s1").unwrap();
        assert_eq!(info.client_info.as_ref().unwrap().name, "cursor");
    }

    // ── end_session ────────────────────────────────────────────

    #[test]
    fn end_session__removes_session_and_reverse_entries() {
        let store = SessionStore::new();
        store.track_request("s1".into(), req_num(1), None);
        store.track_request("s1".into(), req_num(2), None);

        store.end_session("s1");

        assert!(store.get_session("s1").is_none());
        assert!(store.get_session_by_request(&req_num(1)).is_none());
        assert!(store.get_session_by_request(&req_num(2)).is_none());
    }

    #[test]
    fn end_session__leaves_other_sessions_intact() {
        let store = SessionStore::new();
        store.track_request("s1".into(), req_num(1), None);
        store.track_request("s2".into(), req_num(2), None);

        store.end_session("s1");

        assert!(store.get_session("s2").is_some());
        assert_eq!(store.get_session_by_request(&req_num(2)).unwrap().id, "s2");
    }

    #[test]
    fn end_session__missing_id_is_noop() {
        let store = SessionStore::new();
        store.end_session("nope");
        assert!(store.list_sessions().is_empty());
    }

    // ── list_sessions ──────────────────────────────────────────

    #[test]
    fn list_sessions__empty() {
        let store = SessionStore::new();
        assert!(store.list_sessions().is_empty());
    }

    #[test]
    fn list_sessions__sorted_by_last_active_desc() {
        let store = SessionStore::new();
        store.track_request("oldest".into(), req_num(1), None);
        sleep(Duration::from_millis(2));
        store.track_request("middle".into(), req_num(2), None);
        sleep(Duration::from_millis(2));
        store.track_request("newest".into(), req_num(3), None);

        let ids: Vec<_> = store.list_sessions().into_iter().map(|s| s.id).collect();
        assert_eq!(ids, vec!["newest", "middle", "oldest"]);
    }

    #[test]
    fn list_sessions__track_request_promotes_session() {
        let store = SessionStore::new();
        store.track_request("a".into(), req_num(1), None);
        sleep(Duration::from_millis(2));
        store.track_request("b".into(), req_num(2), None);
        sleep(Duration::from_millis(2));
        store.track_request("a".into(), req_num(3), None);

        let ids: Vec<_> = store.list_sessions().into_iter().map(|s| s.id).collect();
        assert_eq!(ids, vec!["a", "b"]);
    }

    // ── get_session ────────────────────────────────────────────

    #[test]
    fn get_session__missing_is_none() {
        let store = SessionStore::new();
        assert!(store.get_session("nope").is_none());
    }

    // ── get_session_by_request ─────────────────────────────────

    #[test]
    fn get_session_by_request__missing_is_none() {
        let store = SessionStore::new();
        assert!(store.get_session_by_request(&req_num(99)).is_none());
    }

    #[test]
    fn get_session_by_request__string_request_id() {
        let store = SessionStore::new();
        let rid = RequestId::String("req-abc".into());
        store.track_request("s1".into(), rid.clone(), None);
        assert_eq!(store.get_session_by_request(&rid).unwrap().id, "s1");
    }

    // ── session_id_from_headers ────────────────────────────────

    fn headers_with_session(value: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(
            axum::http::HeaderName::from_static("mcp-session-id"),
            axum::http::HeaderValue::from_str(value).unwrap(),
        );
        h
    }

    #[test]
    fn session_id_from_headers__present_returns_id() {
        let h = headers_with_session("sess-xyz");
        assert_eq!(session_id_from_headers(&h).as_deref(), Some("sess-xyz"));
    }

    #[test]
    fn session_id_from_headers__missing_returns_none() {
        assert!(session_id_from_headers(&HeaderMap::new()).is_none());
    }

    #[test]
    fn session_id_from_headers__non_utf8_returns_none() {
        let mut h = HeaderMap::new();
        h.insert(
            axum::http::HeaderName::from_static("mcp-session-id"),
            axum::http::HeaderValue::from_bytes(&[0xff, 0xfe]).unwrap(),
        );
        assert!(session_id_from_headers(&h).is_none());
    }
}
