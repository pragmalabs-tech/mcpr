//! `SchemaVersion` — an immutable, content-hashed snapshot of one MCP schema
//! method payload on one upstream server.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

/// Opaque, stable identifier for a `SchemaVersion`.
///
/// The id is the first 16 hex chars of the full SHA-256 content hash —
/// short enough to log, long enough for collision safety across the
/// version counts a proxy will ever hold.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct SchemaVersionId(pub String);

impl SchemaVersionId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for SchemaVersionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A single captured schema payload for one method on one upstream.
///
/// `payload` is the merged `result` field (post-pagination) as JSON.
/// `Arc` wrapping keeps clones cheap when handing versions out of the
/// store to multiple readers.
#[derive(Debug, Clone)]
pub struct SchemaVersion {
    pub id: SchemaVersionId,
    pub upstream_id: String,
    pub method: String,
    pub version: u32,
    pub payload: Arc<Value>,
    pub content_hash: String,
    pub captured_at: DateTime<Utc>,
}

/// Hash a JSON payload to a hex-encoded SHA-256 digest.
///
/// Object keys are sorted recursively before hashing so that two
/// payloads with the same content but different key orders produce
/// the same hash.
pub(crate) fn hash_payload(payload: &Value) -> String {
    let canonical = canonicalize(payload);
    let bytes = serde_json::to_vec(&canonical).expect("canonical json serializes");
    let digest = Sha256::digest(&bytes);
    hex_encode(&digest)
}

fn canonicalize(v: &Value) -> Value {
    match v {
        Value::Object(m) => {
            let mut entries: Vec<(String, Value)> = m
                .iter()
                .map(|(k, v)| (k.clone(), canonicalize(v)))
                .collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            Value::Object(entries.into_iter().collect())
        }
        Value::Array(a) => Value::Array(a.iter().map(canonicalize).collect()),
        other => other.clone(),
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(&mut s, "{b:02x}");
    }
    s
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn hash_payload__stable_across_key_order() {
        let a = json!({"tools": [{"name": "x", "description": "d"}]});
        let b = json!({"tools": [{"description": "d", "name": "x"}]});
        assert_eq!(hash_payload(&a), hash_payload(&b));
    }

    #[test]
    fn hash_payload__differs_on_value_change() {
        let a = json!({"tools": [{"name": "x", "description": "old"}]});
        let b = json!({"tools": [{"name": "x", "description": "new"}]});
        assert_ne!(hash_payload(&a), hash_payload(&b));
    }

    #[test]
    fn hash_payload__differs_on_item_added() {
        let a = json!({"tools": [{"name": "x"}]});
        let b = json!({"tools": [{"name": "x"}, {"name": "y"}]});
        assert_ne!(hash_payload(&a), hash_payload(&b));
    }

    #[test]
    fn schema_version_id__display_roundtrip() {
        let id = SchemaVersionId("abc123".to_string());
        assert_eq!(id.to_string(), "abc123");
        assert_eq!(id.as_str(), "abc123");
    }

    #[test]
    fn hex_encode__known_bytes() {
        assert_eq!(hex_encode(&[0x00, 0xff, 0x10]), "00ff10");
    }

    #[test]
    fn hash_payload__empty_object_is_deterministic() {
        let a = json!({});
        assert_eq!(hash_payload(&a), hash_payload(&a));
    }
}
