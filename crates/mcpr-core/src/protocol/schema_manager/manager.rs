//! `SchemaManager` — top-level per-upstream view of an MCP server's schema.
//!
//! Callers feed schema-method responses in via [`SchemaManager::ingest`].
//! The manager handles pagination buffering, change detection (by content
//! hash), and version assignment, persisting new versions to a
//! [`SchemaStore`]. Query methods read back the latest merged payload
//! without re-hitting the store per item.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use chrono::{DateTime, Utc};
use dashmap::DashMap;
use serde_json::Value;
use tokio::sync::Notify;

use super::store::SchemaStore;
use super::version::{SchemaVersion, SchemaVersionId, hash_payload};
use crate::protocol::schema::{PageStatus, detect_page_status, merge_pages};

/// Tracks in-flight `spawn_ingest` tasks so callers (shutdown handlers,
/// tests) can wait until the async ingest queue has drained.
#[derive(Default)]
struct PendingTracker {
    count: AtomicUsize,
    notify: Notify,
}

impl PendingTracker {
    fn begin(&self) {
        self.count.fetch_add(1, Ordering::SeqCst);
    }
    fn end(&self) {
        if self.count.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.notify.notify_waiters();
        }
    }
    async fn wait_idle(&self) {
        while self.count.load(Ordering::SeqCst) > 0 {
            let notified = self.notify.notified();
            if self.count.load(Ordering::SeqCst) == 0 {
                return;
            }
            notified.await;
        }
    }
}

/// Per-method runtime state held in memory. Separate from the
/// `SchemaStore` because these fields serve the hot path — change
/// detection, pagination buffering, stale flag — and would be expensive
/// to read-through on every ingest.
#[derive(Default)]
struct MethodState {
    page_buffer: Vec<Value>,
    current_hash: Option<String>,
    next_version_number: u32,
    stale: bool,
    stale_since: Option<DateTime<Utc>>,
}

/// Top-level handle for one upstream MCP server's schema view.
///
/// Generic over the store backend; downstream typically uses
/// `SchemaManager<MemorySchemaStore>` for the OSS proxy and swaps in a
/// database-backed store for cloud deployments.
pub struct SchemaManager<S: SchemaStore> {
    upstream_id: String,
    store: S,
    state: Arc<DashMap<String, MethodState>>,
    pending: Arc<PendingTracker>,
}

impl<S: SchemaStore> SchemaManager<S> {
    pub fn new(upstream_id: impl Into<String>, store: S) -> Self {
        Self {
            upstream_id: upstream_id.into(),
            store,
            state: Arc::new(DashMap::new()),
            pending: Arc::new(PendingTracker::default()),
        }
    }

    /// Wait until every task spawned via [`spawn_ingest`] has finished.
    ///
    /// Used by shutdown/test code so the bus sees every
    /// `SchemaVersionCreated` event before it drains.
    pub async fn wait_idle(&self) {
        self.pending.wait_idle().await;
    }

    pub fn upstream_id(&self) -> &str {
        &self.upstream_id
    }

    /// Seed the in-memory state for `method` from the store.
    ///
    /// Callers normally don't need to invoke this directly — `ingest`
    /// lazy-warms on the first call for a method. Exposed for explicit
    /// startup warm-up when desired.
    pub async fn warm(&self, method: &str) {
        let latest = self
            .store
            .latest_version_for_method(&self.upstream_id, method)
            .await;
        if let Some(latest) = latest {
            let mut entry = self.state.entry(method.to_string()).or_default();
            if entry.current_hash.is_none() {
                entry.current_hash = Some(latest.content_hash.clone());
                entry.next_version_number = latest.version + 1;
            }
        }
    }

    /// Bootstrap in-memory state from a pre-existing `SchemaVersion`
    /// (typically loaded from an external persistent store at startup).
    ///
    /// Seeds `current_hash` + `next_version_number` so subsequent
    /// `ingest` calls with matching content return `None` (no phantom
    /// new version) and non-matching content increments from
    /// `version.version + 1`. Also writes the version into the
    /// manager's in-process store so `latest` / `list_tools` /
    /// `get_tool` / etc. see it without needing the first live request.
    ///
    /// Idempotent per method: if `current_hash` is already set (either
    /// from a prior preload or a completed ingest), this is a no-op.
    pub async fn preload(&self, version: SchemaVersion) {
        {
            let mut entry = self.state.entry(version.method.clone()).or_default();
            if entry.current_hash.is_some() {
                return;
            }
            entry.current_hash = Some(version.content_hash.clone());
            entry.next_version_number = version.version.saturating_add(1);
        }
        self.store.put_version(version).await;
    }

    /// Feed a schema-method response through the manager.
    ///
    /// Returns `Some(version)` when a new `SchemaVersion` was created
    /// (pagination complete AND content differs from the current
    /// version). Returns `None` when:
    ///
    /// - The response is not a complete page (still buffering).
    /// - The content hash matches the current version.
    /// - The response has no `result` field.
    pub async fn ingest(
        &self,
        method: &str,
        request_body: &Value,
        response_body: &Value,
    ) -> Option<SchemaVersion> {
        let result = response_body.get("result")?;
        let status = detect_page_status(request_body, response_body);

        let merged = {
            let mut entry = self.state.entry(method.to_string()).or_default();
            entry.page_buffer.push(result.clone());
            match status {
                PageStatus::Complete | PageStatus::LastPage => {
                    let pages = std::mem::take(&mut entry.page_buffer);
                    merge_pages(method, &pages)
                        .unwrap_or_else(|| pages.into_iter().next().unwrap_or(Value::Null))
                }
                PageStatus::FirstPage | PageStatus::MiddlePage => return None,
            }
        };

        let hash = hash_payload(&merged);

        let needs_warm = self
            .state
            .get(method)
            .map(|e| e.current_hash.is_none() && e.next_version_number == 0)
            .unwrap_or(true);
        if needs_warm {
            self.warm(method).await;
        }

        let (same, version_number) = {
            let mut entry = self.state.entry(method.to_string()).or_default();
            if entry.current_hash.as_deref() == Some(hash.as_str()) {
                (true, 0)
            } else {
                let num = entry.next_version_number.max(1);
                entry.current_hash = Some(hash.clone());
                entry.next_version_number = num.saturating_add(1);
                entry.stale = false;
                entry.stale_since = None;
                (false, num)
            }
        };

        if same {
            return None;
        }

        let id = SchemaVersionId(hash.chars().take(16).collect());
        let version = SchemaVersion {
            id,
            upstream_id: self.upstream_id.clone(),
            method: method.to_string(),
            version: version_number,
            payload: Arc::new(merged),
            content_hash: hash,
            captured_at: Utc::now(),
        };
        Some(self.store.put_version(version).await)
    }

    /// Spawn an async ingest task so the caller's hot path does not
    /// pay for merge/hash/store work.
    ///
    /// Returns immediately after spawning. Use [`wait_idle`] to block
    /// until every spawned task (including this one) has completed.
    ///
    /// The caller provides a sink closure that receives the new
    /// [`SchemaVersion`] when one is produced (for emitting events).
    /// The closure runs on the spawned task, not on the caller.
    pub fn spawn_ingest<F>(
        self: &Arc<Self>,
        method: String,
        request_body: Value,
        response_body: Value,
        on_version: F,
    ) where
        F: FnOnce(&SchemaVersion) + Send + 'static,
    {
        self.pending.begin();
        let manager = Arc::clone(self);
        tokio::spawn(async move {
            let result = manager.ingest(&method, &request_body, &response_body).await;
            if let Some(version) = result.as_ref() {
                on_version(version);
            }
            manager.pending.end();
        });
    }

    /// Latest stored version for `method`, or `None` if nothing has
    /// been ingested yet.
    pub async fn latest(&self, method: &str) -> Option<SchemaVersion> {
        self.store
            .latest_version_for_method(&self.upstream_id, method)
            .await
    }

    pub async fn list_tools(&self) -> Vec<Value> {
        self.list_items("tools/list", "tools").await
    }

    pub async fn list_resources(&self) -> Vec<Value> {
        self.list_items("resources/list", "resources").await
    }

    pub async fn list_resource_templates(&self) -> Vec<Value> {
        self.list_items("resources/templates/list", "resourceTemplates")
            .await
    }

    pub async fn list_prompts(&self) -> Vec<Value> {
        self.list_items("prompts/list", "prompts").await
    }

    pub async fn get_tool(&self, name: &str) -> Option<Value> {
        self.find_item_by_field("tools/list", "tools", "name", name)
            .await
    }

    pub async fn get_resource(&self, uri: &str) -> Option<Value> {
        self.find_item_by_field("resources/list", "resources", "uri", uri)
            .await
    }

    pub async fn get_prompt(&self, name: &str) -> Option<Value> {
        self.find_item_by_field("prompts/list", "prompts", "name", name)
            .await
    }

    /// Mark the current version for `method` as stale. Idempotent.
    ///
    /// Sync on purpose — the stale flag is used by the hot request
    /// path (observing `notifications/tools/list_changed`) where a
    /// round-trip to async code would be overkill.
    pub fn mark_stale(&self, method: &str) {
        let mut entry = self.state.entry(method.to_string()).or_default();
        if !entry.stale {
            entry.stale = true;
            entry.stale_since = Some(Utc::now());
        }
    }

    pub fn is_stale(&self, method: &str) -> bool {
        self.state.get(method).map(|e| e.stale).unwrap_or(false)
    }

    pub fn stale_since(&self, method: &str) -> Option<DateTime<Utc>> {
        self.state.get(method).and_then(|e| e.stale_since)
    }

    // ── internals ──

    async fn list_items(&self, method: &str, array_key: &str) -> Vec<Value> {
        let Some(latest) = self.latest(method).await else {
            return Vec::new();
        };
        latest
            .payload
            .get(array_key)
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default()
    }

    async fn find_item_by_field(
        &self,
        method: &str,
        array_key: &str,
        field: &str,
        needle: &str,
    ) -> Option<Value> {
        let latest = self.latest(method).await?;
        let arr = latest.payload.get(array_key).and_then(|v| v.as_array())?;
        arr.iter()
            .find(|item| item.get(field).and_then(|v| v.as_str()) == Some(needle))
            .cloned()
    }
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;
    use crate::protocol::schema_manager::store::MemorySchemaStore;
    use serde_json::json;

    fn manager() -> SchemaManager<MemorySchemaStore> {
        SchemaManager::new("proxy-1", MemorySchemaStore::new())
    }

    fn tools_list_req(cursor: Option<&str>) -> Value {
        match cursor {
            Some(c) => {
                json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {"cursor": c}})
            }
            None => json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
        }
    }

    fn tools_list_resp(tools: Value, next_cursor: Option<&str>) -> Value {
        let mut result = json!({"tools": tools});
        if let Some(c) = next_cursor {
            result["nextCursor"] = json!(c);
        }
        json!({"jsonrpc": "2.0", "id": 1, "result": result})
    }

    #[tokio::test]
    async fn ingest__complete_page_creates_version_one() {
        let m = manager();
        let req = tools_list_req(None);
        let resp = tools_list_resp(json!([{"name": "search"}]), None);
        let v = m.ingest("tools/list", &req, &resp).await.unwrap();
        assert_eq!(v.version, 1);
        assert_eq!(v.method, "tools/list");
        assert_eq!(v.upstream_id, "proxy-1");
    }

    #[tokio::test]
    async fn ingest__first_page_buffers_returns_none() {
        let m = manager();
        let req = tools_list_req(None);
        let resp = tools_list_resp(json!([{"name": "a"}]), Some("cur1"));
        assert!(m.ingest("tools/list", &req, &resp).await.is_none());
    }

    #[tokio::test]
    async fn ingest__first_middle_last_chain_merges_once() {
        let m = manager();

        let r1 = tools_list_resp(json!([{"name": "a"}]), Some("c1"));
        assert!(
            m.ingest("tools/list", &tools_list_req(None), &r1)
                .await
                .is_none()
        );

        let r2 = tools_list_resp(json!([{"name": "b"}]), Some("c2"));
        assert!(
            m.ingest("tools/list", &tools_list_req(Some("c1")), &r2)
                .await
                .is_none()
        );

        let r3 = tools_list_resp(json!([{"name": "c"}]), None);
        let v = m
            .ingest("tools/list", &tools_list_req(Some("c2")), &r3)
            .await
            .unwrap();

        let names: Vec<&str> = v.payload["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["a", "b", "c"]);
        assert_eq!(v.version, 1);
    }

    #[tokio::test]
    async fn ingest__unchanged_payload_returns_none() {
        let m = manager();
        let req = tools_list_req(None);
        let resp = tools_list_resp(json!([{"name": "a"}]), None);
        m.ingest("tools/list", &req, &resp).await.unwrap();
        assert!(m.ingest("tools/list", &req, &resp).await.is_none());
    }

    #[tokio::test]
    async fn preload__seeds_hash_and_version_counter() {
        // Simulates startup hydration: we hand the manager a v3 version
        // that was persisted before a restart. The next ingest with the
        // same content must not mint v4, and the next ingest with
        // different content must mint v4 (not v1).
        let m = manager();
        let req = tools_list_req(None);
        let stored = json!({"tools": [{"name": "a"}]});
        let version = SchemaVersion {
            id: SchemaVersionId("preload-seed-123".to_string()),
            upstream_id: "proxy-1".to_string(),
            method: "tools/list".to_string(),
            version: 3,
            payload: Arc::new(stored.clone()),
            content_hash: hash_payload(&stored),
            captured_at: Utc::now(),
        };
        m.preload(version).await;

        // Same-content ingest: no new version.
        let same = tools_list_resp(json!([{"name": "a"}]), None);
        assert!(m.ingest("tools/list", &req, &same).await.is_none());

        // Different content: increments to v4, not v1.
        let changed = tools_list_resp(json!([{"name": "a"}, {"name": "b"}]), None);
        let v4 = m.ingest("tools/list", &req, &changed).await.unwrap();
        assert_eq!(v4.version, 4);
    }

    #[tokio::test]
    async fn preload__idempotent_second_call_noop() {
        let m = manager();
        let stored = json!({"tools": [{"name": "a"}]});
        let mk = |v: u32, tag: &str| SchemaVersion {
            id: SchemaVersionId(format!("id-{tag}")),
            upstream_id: "proxy-1".to_string(),
            method: "tools/list".to_string(),
            version: v,
            payload: Arc::new(stored.clone()),
            content_hash: format!("hash-{tag}"),
            captured_at: Utc::now(),
        };

        m.preload(mk(3, "first")).await;
        m.preload(mk(99, "second")).await;

        // Second preload was skipped (state already had a hash), so
        // the counter is 4, not 100.
        let req = tools_list_req(None);
        let changed = tools_list_resp(json!([{"name": "b"}]), None);
        let v = m.ingest("tools/list", &req, &changed).await.unwrap();
        assert_eq!(v.version, 4);
    }

    #[tokio::test]
    async fn preload__makes_list_tools_visible_without_ingest() {
        let m = manager();
        let stored = json!({"tools": [{"name": "a"}, {"name": "b"}]});
        let version = SchemaVersion {
            id: SchemaVersionId("preload-list".to_string()),
            upstream_id: "proxy-1".to_string(),
            method: "tools/list".to_string(),
            version: 1,
            payload: Arc::new(stored.clone()),
            content_hash: hash_payload(&stored),
            captured_at: Utc::now(),
        };
        m.preload(version).await;

        let tools = m.list_tools().await;
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0]["name"], "a");
    }

    #[tokio::test]
    async fn ingest__volatile_meta_does_not_create_new_version() {
        // Regression: dashboards saw 138 versions for a server whose tools
        // hadn't changed in weeks, because the server regenerated `_meta`
        // per request. Only the array of items should influence the hash.
        let m = manager();
        let req = tools_list_req(None);

        let r1 = json!({
            "jsonrpc": "2.0", "id": 1,
            "result": {
                "tools": [{"name": "a"}],
                "_meta": {"requestId": "uuid-1"}
            }
        });
        let r2 = json!({
            "jsonrpc": "2.0", "id": 1,
            "result": {
                "tools": [{"name": "a"}],
                "_meta": {"requestId": "uuid-2"}
            }
        });

        let v1 = m.ingest("tools/list", &req, &r1).await.unwrap();
        assert_eq!(v1.version, 1);
        assert!(
            m.ingest("tools/list", &req, &r2).await.is_none(),
            "different _meta with identical tools must not mint a new version"
        );
    }

    #[tokio::test]
    async fn ingest__changed_payload_increments_version() {
        let m = manager();
        let req = tools_list_req(None);
        let r1 = tools_list_resp(json!([{"name": "a"}]), None);
        let v1 = m.ingest("tools/list", &req, &r1).await.unwrap();
        assert_eq!(v1.version, 1);

        let r2 = tools_list_resp(json!([{"name": "a"}, {"name": "b"}]), None);
        let v2 = m.ingest("tools/list", &req, &r2).await.unwrap();
        assert_eq!(v2.version, 2);
    }

    #[tokio::test]
    async fn ingest__clears_stale_on_new_version() {
        let m = manager();
        let req = tools_list_req(None);
        let r1 = tools_list_resp(json!([{"name": "a"}]), None);
        m.ingest("tools/list", &req, &r1).await.unwrap();

        m.mark_stale("tools/list");
        assert!(m.is_stale("tools/list"));

        let r2 = tools_list_resp(json!([{"name": "a"}, {"name": "b"}]), None);
        m.ingest("tools/list", &req, &r2).await.unwrap();
        assert!(!m.is_stale("tools/list"));
    }

    #[tokio::test]
    async fn ingest__no_result_returns_none() {
        let m = manager();
        let req = tools_list_req(None);
        let err_resp =
            json!({"jsonrpc": "2.0", "id": 1, "error": {"code": -32603, "message": "x"}});
        assert!(m.ingest("tools/list", &req, &err_resp).await.is_none());
    }

    #[tokio::test]
    async fn mark_stale__and_is_stale_idempotent() {
        let m = manager();
        assert!(!m.is_stale("tools/list"));
        m.mark_stale("tools/list");
        let first = m.stale_since("tools/list");
        m.mark_stale("tools/list");
        let second = m.stale_since("tools/list");
        assert!(m.is_stale("tools/list"));
        assert_eq!(first, second);
    }

    #[tokio::test]
    async fn list_tools__empty_when_no_version() {
        let m = manager();
        assert!(m.list_tools().await.is_empty());
    }

    #[tokio::test]
    async fn list_tools__returns_items_from_latest() {
        let m = manager();
        let req = tools_list_req(None);
        let resp = tools_list_resp(json!([{"name": "a"}, {"name": "b"}]), None);
        m.ingest("tools/list", &req, &resp).await.unwrap();

        let tools = m.list_tools().await;
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0]["name"], "a");
        assert_eq!(tools[1]["name"], "b");
    }

    #[tokio::test]
    async fn get_tool__by_name_hit_and_miss() {
        let m = manager();
        let req = tools_list_req(None);
        let resp = tools_list_resp(json!([{"name": "search", "description": "find"}]), None);
        m.ingest("tools/list", &req, &resp).await.unwrap();

        let hit = m.get_tool("search").await.unwrap();
        assert_eq!(hit["description"], "find");
        assert!(m.get_tool("missing").await.is_none());
    }

    #[tokio::test]
    async fn get_resource__by_uri() {
        let m = manager();
        let req = json!({"jsonrpc": "2.0", "id": 1, "method": "resources/list"});
        let resp = json!({
            "jsonrpc": "2.0", "id": 1,
            "result": {"resources": [{"uri": "file://a", "name": "A"}]}
        });
        m.ingest("resources/list", &req, &resp).await.unwrap();
        let r = m.get_resource("file://a").await.unwrap();
        assert_eq!(r["name"], "A");
    }

    #[tokio::test]
    async fn warm__seeds_counter_from_store() {
        let store = MemorySchemaStore::new();
        let pre = SchemaVersion {
            id: SchemaVersionId("abc".to_string()),
            upstream_id: "proxy-1".to_string(),
            method: "tools/list".to_string(),
            version: 5,
            payload: Arc::new(json!({"tools": [{"name": "x"}]})),
            content_hash: "prior-hash".to_string(),
            captured_at: Utc::now(),
        };
        store.put_version(pre).await;

        let m = SchemaManager::new("proxy-1", store);
        let req = tools_list_req(None);
        let resp = tools_list_resp(json!([{"name": "y"}]), None);
        let v = m.ingest("tools/list", &req, &resp).await.unwrap();
        assert_eq!(v.version, 6);
    }

    #[tokio::test]
    async fn latest__returns_current_version() {
        let m = manager();
        let req = tools_list_req(None);
        let resp = tools_list_resp(json!([{"name": "a"}]), None);
        m.ingest("tools/list", &req, &resp).await.unwrap();
        let latest = m.latest("tools/list").await.unwrap();
        assert_eq!(latest.version, 1);
    }

    #[tokio::test]
    async fn list_resource_templates__walks_template_key() {
        let m = manager();
        let req = json!({"jsonrpc": "2.0", "id": 1, "method": "resources/templates/list"});
        let resp = json!({
            "jsonrpc": "2.0", "id": 1,
            "result": {"resourceTemplates": [{"uriTemplate": "file://{id}", "name": "f"}]}
        });
        m.ingest("resources/templates/list", &req, &resp)
            .await
            .unwrap();
        let items = m.list_resource_templates().await;
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["name"], "f");
    }

    #[tokio::test]
    async fn upstream_id__accessor() {
        let m = manager();
        assert_eq!(m.upstream_id(), "proxy-1");
    }
}
