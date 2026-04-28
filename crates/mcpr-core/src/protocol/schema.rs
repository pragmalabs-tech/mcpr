//! Schema state for an upstream MCP server.
//!
//! `Schema` accumulates the identity-defining fields of an upstream's
//! tools, prompts, resources, and resource templates. `add_*` methods
//! return `true` if the schema actually changed — wiring version bumps
//! to real API changes, not description tweaks or per-request metadata.
//!
//! Stored fields per spec table:
//!
//! | Item              | Stored                |
//! |-------------------|-----------------------|
//! | tool              | `name`, `inputSchema` |
//! | prompt            | `name`, `arguments`   |
//! | resource          | `name`, `uri`         |
//! | resource template | `name`, `uriTemplate` |

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fmt::Write;

use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Schema {
    pub tools: BTreeMap<String, Tool>,
    pub prompts: BTreeMap<String, Prompt>,
    pub resources: BTreeMap<String, Resource>,
    pub resource_templates: BTreeMap<String, ResourceTemplate>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tool {
    pub input_schema: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Prompt {
    pub arguments: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Resource {
    pub uri: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResourceTemplate {
    pub uri_template: String,
}

/// Outcome of a successful `add_*` mutation.
#[derive(Debug, Clone, PartialEq)]
pub struct SchemaChange {
    /// `content_hash()` after the mutation.
    pub hash: String,
    pub reason: ChangeReason,
}

/// What changed in the most recent mutation. Carries the affected
/// item's name so the caller can emit a structured event.
#[derive(Debug, Clone, PartialEq)]
pub enum ChangeReason {
    ToolAdded { name: String },
    ToolModified { name: String },
    PromptAdded { name: String },
    PromptModified { name: String },
    ResourceAdded { name: String },
    ResourceModified { name: String },
    ResourceTemplateAdded { name: String },
    ResourceTemplateModified { name: String },
}

impl Schema {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert or update a tool. Returns `Some(SchemaChange)` if the
    /// schema changed, `None` if the new value matches the existing one.
    pub fn add_tool(&mut self, name: String, input_schema: Value) -> Option<SchemaChange> {
        let new = Tool { input_schema };
        let reason = match self.tools.get(&name) {
            Some(existing) if *existing == new => return None,
            Some(_) => ChangeReason::ToolModified { name: name.clone() },
            None => ChangeReason::ToolAdded { name: name.clone() },
        };
        self.tools.insert(name, new);
        Some(SchemaChange {
            hash: self.content_hash(),
            reason,
        })
    }

    /// Insert or update a prompt.
    pub fn add_prompt(&mut self, name: String, arguments: Value) -> Option<SchemaChange> {
        let new = Prompt { arguments };
        let reason = match self.prompts.get(&name) {
            Some(existing) if *existing == new => return None,
            Some(_) => ChangeReason::PromptModified { name: name.clone() },
            None => ChangeReason::PromptAdded { name: name.clone() },
        };
        self.prompts.insert(name, new);
        Some(SchemaChange {
            hash: self.content_hash(),
            reason,
        })
    }

    /// Insert or update a resource.
    pub fn add_resource(&mut self, name: String, uri: String) -> Option<SchemaChange> {
        let new = Resource { uri };
        let reason = match self.resources.get(&name) {
            Some(existing) if *existing == new => return None,
            Some(_) => ChangeReason::ResourceModified { name: name.clone() },
            None => ChangeReason::ResourceAdded { name: name.clone() },
        };
        self.resources.insert(name, new);
        Some(SchemaChange {
            hash: self.content_hash(),
            reason,
        })
    }

    /// Insert or update a resource template.
    pub fn add_resource_template(
        &mut self,
        name: String,
        uri_template: String,
    ) -> Option<SchemaChange> {
        let new = ResourceTemplate { uri_template };
        let reason = match self.resource_templates.get(&name) {
            Some(existing) if *existing == new => return None,
            Some(_) => ChangeReason::ResourceTemplateModified { name: name.clone() },
            None => ChangeReason::ResourceTemplateAdded { name: name.clone() },
        };
        self.resource_templates.insert(name, new);
        Some(SchemaChange {
            hash: self.content_hash(),
            reason,
        })
    }

    /// SHA-256 hex of the canonical JSON. Stable across insertion order
    /// because every container is a `BTreeMap` and serde_json's default
    /// `Map` is also key-sorted.
    pub fn content_hash(&self) -> String {
        let canonical = serde_json::to_vec(self).expect("Schema is serializable");
        let digest = Sha256::digest(&canonical);
        let mut hex = String::with_capacity(64);
        for b in digest.iter() {
            write!(hex, "{b:02x}").expect("write to String never fails");
        }
        hex
    }
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── add_tool ─────────────────────────────────────────────

    #[test]
    fn add_tool__first_insert_reports_added_with_latest_hash() {
        let mut s = Schema::new();
        let change = s.add_tool("a".into(), json!({"type": "object"})).unwrap();
        assert_eq!(change.reason, ChangeReason::ToolAdded { name: "a".into() });
        assert_eq!(change.hash, s.content_hash());
    }

    #[test]
    fn add_tool__identical_reinsert_returns_none() {
        let mut s = Schema::new();
        s.add_tool("a".into(), json!({"type": "object"}));
        assert!(s.add_tool("a".into(), json!({"type": "object"})).is_none());
    }

    #[test]
    fn add_tool__input_schema_change_reports_modified() {
        let mut s = Schema::new();
        s.add_tool("a".into(), json!({"type": "object"}));
        let change = s
            .add_tool(
                "a".into(),
                json!({"type": "object", "properties": {"q": {"type": "string"}}}),
            )
            .unwrap();
        assert_eq!(
            change.reason,
            ChangeReason::ToolModified { name: "a".into() }
        );
    }

    // ── add_prompt ───────────────────────────────────────────

    #[test]
    fn add_prompt__first_insert_reports_added() {
        let mut s = Schema::new();
        let change = s
            .add_prompt("greet".into(), json!([{"name": "topic", "required": true}]))
            .unwrap();
        assert_eq!(
            change.reason,
            ChangeReason::PromptAdded {
                name: "greet".into()
            }
        );
    }

    #[test]
    fn add_prompt__identical_reinsert_returns_none() {
        let mut s = Schema::new();
        let args = json!([{"name": "topic", "required": true}]);
        s.add_prompt("greet".into(), args.clone());
        assert!(s.add_prompt("greet".into(), args).is_none());
    }

    // ── add_resource ─────────────────────────────────────────

    #[test]
    fn add_resource__first_insert_reports_added() {
        let mut s = Schema::new();
        let change = s.add_resource("r".into(), "file:///a".into()).unwrap();
        assert_eq!(
            change.reason,
            ChangeReason::ResourceAdded { name: "r".into() }
        );
    }

    #[test]
    fn add_resource__uri_change_reports_modified() {
        let mut s = Schema::new();
        s.add_resource("r".into(), "file:///a".into());
        let change = s.add_resource("r".into(), "file:///b".into()).unwrap();
        assert_eq!(
            change.reason,
            ChangeReason::ResourceModified { name: "r".into() }
        );
    }

    // ── add_resource_template ────────────────────────────────

    #[test]
    fn add_resource_template__first_insert_reports_added() {
        let mut s = Schema::new();
        let change = s
            .add_resource_template("doc".into(), "doc://{id}".into())
            .unwrap();
        assert_eq!(
            change.reason,
            ChangeReason::ResourceTemplateAdded { name: "doc".into() }
        );
    }

    #[test]
    fn add_resource_template__identical_reinsert_returns_none() {
        let mut s = Schema::new();
        s.add_resource_template("doc".into(), "doc://{id}".into());
        assert!(
            s.add_resource_template("doc".into(), "doc://{id}".into())
                .is_none()
        );
    }

    #[test]
    fn add__returned_hash_matches_post_mutation_hash() {
        let mut s = Schema::new();
        let change = s.add_tool("t".into(), json!({"type": "object"})).unwrap();
        assert_eq!(change.hash, s.content_hash());
    }

    // ── content_hash ─────────────────────────────────────────

    #[test]
    fn content_hash__stable_across_insertion_order() {
        let mut a = Schema::new();
        a.add_tool("a".into(), json!({"type": "object"}));
        a.add_tool("b".into(), json!({"type": "object"}));

        let mut b = Schema::new();
        b.add_tool("b".into(), json!({"type": "object"}));
        b.add_tool("a".into(), json!({"type": "object"}));

        assert_eq!(a.content_hash(), b.content_hash());
    }

    #[test]
    fn content_hash__changes_when_tool_input_schema_changes() {
        let mut a = Schema::new();
        a.add_tool("t".into(), json!({"type": "object"}));

        let mut b = Schema::new();
        b.add_tool(
            "t".into(),
            json!({"type": "object", "properties": {"q": {"type": "string"}}}),
        );

        assert_ne!(a.content_hash(), b.content_hash());
    }

    #[test]
    fn content_hash__changes_when_tool_added() {
        let mut a = Schema::new();
        a.add_tool("t1".into(), json!({"type": "object"}));
        let h_before = a.content_hash();

        a.add_tool("t2".into(), json!({"type": "object"}));
        assert_ne!(a.content_hash(), h_before);
    }

    #[test]
    fn content_hash__empty_schema_is_deterministic() {
        assert_eq!(Schema::new().content_hash(), Schema::new().content_hash());
    }

    #[test]
    fn content_hash__nested_object_key_order_is_irrelevant() {
        // serde_json::Map is BTreeMap (no `preserve_order` feature in
        // the workspace), so the key order in the source JSON does not
        // leak into the hash.
        let v1: Value = serde_json::from_str(r#"{"b": 1, "a": {"y": 2, "x": 1}}"#).unwrap();
        let v2: Value = serde_json::from_str(r#"{"a": {"x": 1, "y": 2}, "b": 1}"#).unwrap();

        let mut s1 = Schema::new();
        s1.add_tool("t".into(), v1);
        let mut s2 = Schema::new();
        s2.add_tool("t".into(), v2);

        assert_eq!(s1.content_hash(), s2.content_hash());
    }

    #[test]
    fn content_hash__real_input_schema_round_trip_is_stable() {
        // The CreateClozeQuestionArgs schema, parsed twice from the
        // same source. Hashes must match.
        let src = r##"{
            "$defs": {
                "ClozeBlank": {
                    "properties": {
                        "correct_answers": {
                            "description": "INTERNAL: do not reveal in your response.",
                            "items": {"type": "string"},
                            "type": "array"
                        },
                        "id": {"type": "string"}
                    },
                    "required": ["id", "correct_answers"],
                    "type": "object"
                }
            },
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "properties": {
                "blanks": {
                    "items": {"$ref": "#/$defs/ClozeBlank"},
                    "type": "array"
                },
                "text": {"type": "string"}
            },
            "required": ["text", "blanks"],
            "type": "object"
        }"##;

        let mut s1 = Schema::new();
        s1.add_tool("create_cloze".into(), serde_json::from_str(src).unwrap());
        let mut s2 = Schema::new();
        s2.add_tool("create_cloze".into(), serde_json::from_str(src).unwrap());

        assert_eq!(s1.content_hash(), s2.content_hash());
    }
}
