//! Schema state for an upstream MCP server.
//!
//! `Schema` accumulates the upstream's tools, prompts, resources, and
//! resource templates as defined by MCP 2025-11-25. Each item carries
//! every field the spec defines so downstream consumers (TUI, cloud
//! ingest) can render or diff them without re-fetching from upstream.
//!
//! Tracking is currently implemented only for tools. `add_tool` returns
//! `Some(ChangeSchema)` if the inserted tool differs from what's stored
//! (or if it's new), and `None` if the tool is byte-equal to the
//! existing entry. The wrapped `Reason` records how the caller observed
//! the tool — via a `tools/list` response or via a `tools/call`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Schema {
    pub tools: BTreeMap<String, Tool>,
    pub prompts: BTreeMap<String, Prompt>,
    pub resources: BTreeMap<String, Resource>,
    pub resource_templates: BTreeMap<String, ResourceTemplate>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Tool {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub input_schema: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<ToolAnnotations>,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "_meta")]
    pub meta: Option<Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolAnnotations {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_only_hint: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destructive_hint: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotent_hint: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_world_hint: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Prompt {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments: Option<Vec<PromptArgument>>,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "_meta")]
    pub meta: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptArgument {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Resource {
    pub uri: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<Annotations>,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "_meta")]
    pub meta: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResourceTemplate {
    pub uri_template: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<Annotations>,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "_meta")]
    pub meta: Option<Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Annotations {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audience: Option<Vec<Role>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_modified: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

/// How an item entered the schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// Discovered via a list call (e.g. `tools/list`).
    Added,
    /// Observed via a direct call (e.g. `tools/call`).
    Observed,
}

/// A change to the upstream schema. Wraps the affected item alongside
/// the `Reason` describing how the caller observed it.
#[derive(Debug, Clone, PartialEq)]
pub enum ChangeSchema {
    Tool(Reason, Tool),
    Prompt(Reason, Prompt),
    Resource(Reason, Resource),
    ResourceTemplate(Reason, ResourceTemplate),
}

impl Schema {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert or update a tool. Returns `Some(ChangeSchema::Tool)` if
    /// the schema changed, `None` if the new value is byte-equal to the
    /// existing entry.
    pub fn add_tool(&mut self, tool: Tool, reason: Reason) -> Option<ChangeSchema> {
        if self.tools.get(&tool.name) == Some(&tool) {
            return None;
        }
        self.tools.insert(tool.name.clone(), tool.clone());
        Some(ChangeSchema::Tool(reason, tool))
    }
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool(name: &str) -> Tool {
        Tool {
            name: name.into(),
            title: None,
            description: None,
            input_schema: json!({"type": "object"}),
            output_schema: None,
            annotations: None,
            meta: None,
        }
    }

    // ── add_tool ─────────────────────────────────────────────

    #[test]
    fn add_tool__new_tool_returns_change_with_reason() {
        let mut s = Schema::new();
        let change = s.add_tool(tool("a"), Reason::Added).unwrap();
        assert_eq!(change, ChangeSchema::Tool(Reason::Added, tool("a")));
    }

    #[test]
    fn add_tool__identical_reinsert_returns_none() {
        let mut s = Schema::new();
        s.add_tool(tool("a"), Reason::Added);
        assert!(s.add_tool(tool("a"), Reason::Added).is_none());
    }

    #[test]
    fn add_tool__identical_reinsert_with_different_reason_returns_none() {
        let mut s = Schema::new();
        s.add_tool(tool("a"), Reason::Added);
        assert!(s.add_tool(tool("a"), Reason::Observed).is_none());
    }

    #[test]
    fn add_tool__input_schema_change_replaces() {
        let mut s = Schema::new();
        s.add_tool(tool("a"), Reason::Added);
        let mut updated = tool("a");
        updated.input_schema = json!({"type": "object", "properties": {"q": {"type": "string"}}});
        let change = s.add_tool(updated.clone(), Reason::Added).unwrap();
        assert_eq!(change, ChangeSchema::Tool(Reason::Added, updated.clone()));
        assert_eq!(s.tools.get("a"), Some(&updated));
    }

    #[test]
    fn add_tool__title_change_replaces() {
        let mut s = Schema::new();
        s.add_tool(tool("a"), Reason::Added);
        let mut updated = tool("a");
        updated.title = Some("Alpha".into());
        let change = s.add_tool(updated.clone(), Reason::Observed).unwrap();
        assert_eq!(change, ChangeSchema::Tool(Reason::Observed, updated));
    }

    #[test]
    fn add_tool__keys_by_tool_name() {
        let mut s = Schema::new();
        s.add_tool(tool("a"), Reason::Added);
        s.add_tool(tool("b"), Reason::Added);
        assert_eq!(s.tools.keys().collect::<Vec<_>>(), vec!["a", "b"]);
    }

    // ── Tool serde (spec field-name compliance) ──────────────

    #[test]
    fn tool__serializes_optional_fields_with_camel_case() {
        let t = Tool {
            name: "search".into(),
            title: Some("Search".into()),
            description: Some("desc".into()),
            input_schema: json!({"type": "object"}),
            output_schema: Some(json!({"type": "string"})),
            annotations: Some(ToolAnnotations {
                read_only_hint: Some(true),
                ..Default::default()
            }),
            meta: Some(json!({"trace": "abc"})),
        };
        let v = serde_json::to_value(&t).unwrap();
        assert!(v.get("inputSchema").is_some());
        assert!(v.get("outputSchema").is_some());
        assert!(v.get("_meta").is_some());
        assert_eq!(v["annotations"]["readOnlyHint"], json!(true));
    }

    #[test]
    fn tool__omits_unset_optional_fields() {
        let t = tool("t");
        let v = serde_json::to_value(&t).unwrap();
        let obj = v.as_object().unwrap();
        let mut keys: Vec<&String> = obj.keys().collect();
        keys.sort();
        assert_eq!(keys, vec!["inputSchema", "name"]);
    }

    #[test]
    fn tool__roundtrip_preserves_all_fields() {
        let t = Tool {
            name: "search".into(),
            title: Some("Search".into()),
            description: Some("desc".into()),
            input_schema: json!({"type": "object"}),
            output_schema: Some(json!({"type": "string"})),
            annotations: Some(ToolAnnotations {
                title: Some("Search".into()),
                read_only_hint: Some(true),
                destructive_hint: Some(false),
                idempotent_hint: Some(true),
                open_world_hint: Some(false),
            }),
            meta: Some(json!({"trace": "abc"})),
        };
        let json = serde_json::to_string(&t).unwrap();
        let back: Tool = serde_json::from_str(&json).unwrap();
        assert_eq!(t, back);
    }

    // ── Prompt / Resource / ResourceTemplate serde ────────────

    #[test]
    fn prompt__roundtrip_preserves_arguments() {
        let p = Prompt {
            name: "greet".into(),
            title: Some("Greet".into()),
            description: Some("Say hi".into()),
            arguments: Some(vec![PromptArgument {
                name: "topic".into(),
                title: None,
                description: Some("subject".into()),
                required: Some(true),
            }]),
            meta: None,
        };
        let json = serde_json::to_string(&p).unwrap();
        assert_eq!(p, serde_json::from_str::<Prompt>(&json).unwrap());
    }

    #[test]
    fn resource__serializes_uri_and_mime_type() {
        let r = Resource {
            uri: "file:///a".into(),
            name: "a".into(),
            title: None,
            description: None,
            mime_type: Some("text/plain".into()),
            size: Some(42),
            annotations: Some(Annotations {
                audience: Some(vec![Role::User]),
                priority: Some(0.5),
                last_modified: Some("2026-01-01T00:00:00Z".into()),
            }),
            meta: None,
        };
        let v = serde_json::to_value(&r).unwrap();
        assert_eq!(v["mimeType"], json!("text/plain"));
        assert_eq!(v["annotations"]["audience"], json!(["user"]));
        assert_eq!(
            v["annotations"]["lastModified"],
            json!("2026-01-01T00:00:00Z")
        );
    }

    #[test]
    fn resource_template__serializes_uri_template() {
        let rt = ResourceTemplate {
            uri_template: "doc://{id}".into(),
            name: "doc".into(),
            title: None,
            description: None,
            mime_type: None,
            annotations: None,
            meta: None,
        };
        let v = serde_json::to_value(&rt).unwrap();
        assert_eq!(v["uriTemplate"], json!("doc://{id}"));
    }
}
