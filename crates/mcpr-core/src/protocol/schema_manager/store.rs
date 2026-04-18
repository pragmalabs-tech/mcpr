//! `SchemaStore` — pluggable persistence for captured `SchemaVersion`s.
//!
//! Mirrors the `SessionStore` pattern: return-position `impl Future`
//! signatures, generic at call sites. Not `dyn`-compatible by design.
//
// TODO(step-2): content-addressed item dedup pool. Today the in-memory
// store holds whole payloads per version; items that appear across
// versions are cloned. A later step can intern tool / resource / prompt
// entries behind the same `SchemaStore` interface.

use std::future::Future;
use std::sync::Arc;

use dashmap::DashMap;

use super::version::{SchemaVersion, SchemaVersionId};

/// Trait for schema version storage backends.
///
/// Implementations must be `Send + Sync + 'static` so a single store
/// can back many concurrent `SchemaManager` readers.
pub trait SchemaStore: Send + Sync + 'static {
    /// Insert a new version. Implementations handle retention / eviction.
    /// Returns the stored version (the caller may not have populated
    /// `version` correctly — implementations may assign it).
    fn put_version(&self, version: SchemaVersion) -> impl Future<Output = SchemaVersion> + Send;

    /// Fetch a specific version by id. `None` if never stored or evicted.
    fn get_version(
        &self,
        id: &SchemaVersionId,
    ) -> impl Future<Output = Option<SchemaVersion>> + Send;

    /// Latest version for a given `(upstream_id, method)`.
    fn latest_version_for_method(
        &self,
        upstream_id: &str,
        method: &str,
    ) -> impl Future<Output = Option<SchemaVersion>> + Send;

    /// All versions for a given `(upstream_id, method)`, newest first.
    /// Bounded by the store's retention policy.
    fn list_versions(
        &self,
        upstream_id: &str,
        method: &str,
    ) -> impl Future<Output = Vec<SchemaVersion>> + Send;

    /// Drop versions older than `keep` for `(upstream_id, method)`.
    /// Idempotent. Default impl is a no-op for stores without explicit
    /// pruning.
    fn prune(
        &self,
        _upstream_id: &str,
        _method: &str,
        _keep: usize,
    ) -> impl Future<Output = ()> + Send {
        async {}
    }
}

type VersionKey = (String, String); // (upstream_id, method)

/// In-memory `SchemaStore` backed by DashMap.
///
/// Each `(upstream_id, method)` key owns a FIFO ring buffer of at most
/// `capacity` versions. On insert overflow, the oldest version is
/// dropped and its id removed from the lookup index.
#[derive(Clone)]
pub struct MemorySchemaStore {
    by_key: Arc<DashMap<VersionKey, Vec<SchemaVersion>>>,
    index: Arc<DashMap<SchemaVersionId, VersionKey>>,
    capacity: usize,
}

impl MemorySchemaStore {
    pub fn new() -> Self {
        Self::with_capacity(20)
    }

    pub fn with_capacity(capacity: usize) -> Self {
        assert!(capacity > 0, "MemorySchemaStore capacity must be > 0");
        Self {
            by_key: Arc::new(DashMap::new()),
            index: Arc::new(DashMap::new()),
            capacity,
        }
    }
}

impl Default for MemorySchemaStore {
    fn default() -> Self {
        Self::new()
    }
}

impl SchemaStore for MemorySchemaStore {
    async fn put_version(&self, version: SchemaVersion) -> SchemaVersion {
        let key: VersionKey = (version.upstream_id.clone(), version.method.clone());
        let id = version.id.clone();

        let mut entry = self.by_key.entry(key.clone()).or_default();
        entry.push(version.clone());
        while entry.len() > self.capacity {
            let evicted = entry.remove(0);
            self.index.remove(&evicted.id);
        }
        drop(entry);

        self.index.insert(id, key);
        version
    }

    async fn get_version(&self, id: &SchemaVersionId) -> Option<SchemaVersion> {
        let key = self.index.get(id)?.clone();
        let ring = self.by_key.get(&key)?;
        ring.iter().find(|v| &v.id == id).cloned()
    }

    async fn latest_version_for_method(
        &self,
        upstream_id: &str,
        method: &str,
    ) -> Option<SchemaVersion> {
        let key = (upstream_id.to_string(), method.to_string());
        let ring = self.by_key.get(&key)?;
        ring.last().cloned()
    }

    async fn list_versions(&self, upstream_id: &str, method: &str) -> Vec<SchemaVersion> {
        let key = (upstream_id.to_string(), method.to_string());
        match self.by_key.get(&key) {
            Some(ring) => ring.iter().rev().cloned().collect(),
            None => Vec::new(),
        }
    }

    async fn prune(&self, upstream_id: &str, method: &str, keep: usize) {
        let key = (upstream_id.to_string(), method.to_string());
        let Some(mut entry) = self.by_key.get_mut(&key) else {
            return;
        };
        while entry.len() > keep {
            let evicted = entry.remove(0);
            self.index.remove(&evicted.id);
        }
    }
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;
    use chrono::Utc;
    use serde_json::json;

    fn version(upstream: &str, method: &str, version: u32, payload_tag: &str) -> SchemaVersion {
        let hash = format!("{method}-{version}-{payload_tag}");
        SchemaVersion {
            id: SchemaVersionId(hash[..hash.len().min(16)].to_string()),
            upstream_id: upstream.to_string(),
            method: method.to_string(),
            version,
            payload: Arc::new(json!({"tag": payload_tag})),
            content_hash: hash,
            captured_at: Utc::now(),
        }
    }

    #[tokio::test]
    async fn put_and_get_version__roundtrip() {
        let store = MemorySchemaStore::new();
        let v = version("p1", "tools/list", 1, "a");
        let stored = store.put_version(v.clone()).await;
        assert_eq!(stored.id, v.id);

        let fetched = store.get_version(&v.id).await.unwrap();
        assert_eq!(fetched.id, v.id);
        assert_eq!(fetched.version, 1);
    }

    #[tokio::test]
    async fn latest_version_for_method__returns_newest() {
        let store = MemorySchemaStore::new();
        store.put_version(version("p1", "tools/list", 1, "a")).await;
        store.put_version(version("p1", "tools/list", 2, "b")).await;
        store.put_version(version("p1", "tools/list", 3, "c")).await;

        let latest = store
            .latest_version_for_method("p1", "tools/list")
            .await
            .unwrap();
        assert_eq!(latest.version, 3);
    }

    #[tokio::test]
    async fn latest_version_for_method__none_when_empty() {
        let store = MemorySchemaStore::new();
        assert!(
            store
                .latest_version_for_method("p1", "tools/list")
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn list_versions__newest_first() {
        let store = MemorySchemaStore::new();
        for i in 1..=3 {
            store
                .put_version(version("p1", "tools/list", i, &i.to_string()))
                .await;
        }
        let all = store.list_versions("p1", "tools/list").await;
        let nums: Vec<u32> = all.iter().map(|v| v.version).collect();
        assert_eq!(nums, vec![3, 2, 1]);
    }

    #[tokio::test]
    async fn ring_buffer__evicts_oldest_at_capacity() {
        let store = MemorySchemaStore::with_capacity(3);
        for i in 1..=5 {
            store
                .put_version(version("p1", "tools/list", i, &i.to_string()))
                .await;
        }
        let all = store.list_versions("p1", "tools/list").await;
        let nums: Vec<u32> = all.iter().map(|v| v.version).collect();
        assert_eq!(nums, vec![5, 4, 3]);
    }

    #[tokio::test]
    async fn ring_buffer__get_version_returns_none_after_eviction() {
        let store = MemorySchemaStore::with_capacity(2);
        let v1 = version("p1", "tools/list", 1, "a");
        store.put_version(v1.clone()).await;
        store.put_version(version("p1", "tools/list", 2, "b")).await;
        store.put_version(version("p1", "tools/list", 3, "c")).await;

        assert!(store.get_version(&v1.id).await.is_none());
    }

    #[tokio::test]
    async fn prune__trims_to_keep_count() {
        let store = MemorySchemaStore::new();
        for i in 1..=5 {
            store
                .put_version(version("p1", "tools/list", i, &i.to_string()))
                .await;
        }
        store.prune("p1", "tools/list", 2).await;
        let all = store.list_versions("p1", "tools/list").await;
        let nums: Vec<u32> = all.iter().map(|v| v.version).collect();
        assert_eq!(nums, vec![5, 4]);
    }

    #[tokio::test]
    async fn prune__noop_when_keep_exceeds_size() {
        let store = MemorySchemaStore::new();
        for i in 1..=3 {
            store
                .put_version(version("p1", "tools/list", i, &i.to_string()))
                .await;
        }
        store.prune("p1", "tools/list", 10).await;
        let all = store.list_versions("p1", "tools/list").await;
        assert_eq!(all.len(), 3);
    }

    #[tokio::test]
    async fn different_methods__isolated() {
        let store = MemorySchemaStore::new();
        store.put_version(version("p1", "tools/list", 1, "t")).await;
        store
            .put_version(version("p1", "prompts/list", 1, "p"))
            .await;

        assert_eq!(store.list_versions("p1", "tools/list").await.len(), 1);
        assert_eq!(store.list_versions("p1", "prompts/list").await.len(), 1);
    }

    #[tokio::test]
    async fn different_upstreams__isolated() {
        let store = MemorySchemaStore::new();
        store.put_version(version("p1", "tools/list", 1, "a")).await;
        store.put_version(version("p2", "tools/list", 1, "b")).await;

        let p1_latest = store
            .latest_version_for_method("p1", "tools/list")
            .await
            .unwrap();
        let p2_latest = store
            .latest_version_for_method("p2", "tools/list")
            .await
            .unwrap();

        assert_eq!(p1_latest.upstream_id, "p1");
        assert_eq!(p2_latest.upstream_id, "p2");
    }

    #[test]
    #[should_panic(expected = "capacity must be > 0")]
    fn with_capacity__rejects_zero() {
        let _ = MemorySchemaStore::with_capacity(0);
    }
}
