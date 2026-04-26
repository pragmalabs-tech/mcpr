//! MCP schema capture: types, pagination merging, and diff logic.
//!
//! This module understands the structure of MCP discovery responses
//! (`initialize`, `tools/list`, `resources/list`, `prompts/list`,
//! `resources/templates/list`) and provides:
//!
//! - **Pagination detection**: Determine if a response is a single page or
//!   part of a paginated sequence (MCP cursor-based pagination).
//! - **Page merging**: Combine paginated responses into a single snapshot.
//! - **Schema diffing**: Compare two snapshots to detect added, removed,
//!   and modified items (tools, resources, prompts).
//!
//! This is pure protocol logic — no HTTP, no storage, no hashing.
//! The proxy and storage layers consume these functions.

use std::collections::HashMap;

use serde::Serialize;
use serde_json::Value;

use super::mcp::{ClientMethod, LifecycleMethod, PromptsMethod, ResourcesMethod, ToolsMethod};

// ── Types ────────────────────────────────────────────────────────────

/// Pagination state for an MCP list response.
///
/// Determined by checking `params.cursor` in the request and
/// `result.nextCursor` in the response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PageStatus {
    /// Single-page response (no pagination). This is the common path.
    Complete,
    /// First page of a paginated response (no cursor in request, has nextCursor).
    FirstPage,
    /// Middle page (has cursor in request and nextCursor in response).
    MiddlePage,
    /// Last page (has cursor in request, no nextCursor in response).
    LastPage,
}

/// Result of diffing two schema snapshots for a single MCP method.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaDiff {
    /// Type of change: "tool_added", "tool_removed", "tool_modified",
    /// "resource_added", "prompt_modified", "updated", etc.
    pub change_type: String,
    /// Name of the affected item (e.g., "search_products"). None for
    /// bulk changes like "updated" or "initial".
    pub item_name: Option<String>,
}

// ── Public functions ─────────────────────────────────────────────────

/// Check if an MCP method is a schema discovery method whose response
/// should be captured.
pub fn is_schema_method(method: &ClientMethod) -> bool {
    matches!(
        method,
        ClientMethod::Lifecycle(LifecycleMethod::Initialize)
            | ClientMethod::Tools(ToolsMethod::List)
            | ClientMethod::Resources(ResourcesMethod::List)
            | ClientMethod::Resources(ResourcesMethod::TemplatesList)
            | ClientMethod::Prompts(PromptsMethod::List)
    )
}

/// Determine pagination status from the request body and response body.
///
/// MCP pagination uses cursor-based paging:
/// - Request `params.cursor` present → continuing from a previous page.
/// - Response `result.nextCursor` present → more pages available.
pub fn detect_page_status(request_body: &Value, response_body: &Value) -> PageStatus {
    let req_has_cursor = request_body
        .get("params")
        .and_then(|p| p.get("cursor"))
        .and_then(|c| c.as_str())
        .is_some();

    let resp_has_next_cursor = response_body
        .get("result")
        .and_then(|r| r.get("nextCursor"))
        .and_then(|c| c.as_str())
        .is_some();

    match (req_has_cursor, resp_has_next_cursor) {
        (false, false) => PageStatus::Complete,
        (false, true) => PageStatus::FirstPage,
        (true, true) => PageStatus::MiddlePage,
        (true, false) => PageStatus::LastPage,
    }
}

/// Merge paginated list responses into a single combined `result` payload.
///
/// Each page is the `result` field from a JSON-RPC response. This function
/// merges the array field (tools, resources, resourceTemplates, prompts)
/// across all pages into a single value.
///
/// Returns `None` if pages is empty or the method has no array key.
pub fn merge_pages(method: &str, pages: &[Value]) -> Option<Value> {
    if pages.is_empty() {
        return None;
    }

    // List methods (tools/list, resources/list, …) must extract only the
    // named array so per-request metadata (`_meta`, server-generated
    // request ids, etc.) does not leak into the hash and produce
    // phantom versions. Non-list methods (initialize) retain the raw
    // page — they have no array to project.
    let Some(array_key) = method_array_key(method) else {
        return (pages.len() == 1).then(|| pages[0].clone());
    };

    let mut merged_array: Vec<Value> = Vec::new();
    for page in pages {
        if let Some(arr) = page.get(array_key).and_then(|a| a.as_array()) {
            merged_array.extend(arr.iter().cloned());
        }
    }

    // Sort by `name` so identical item sets in different upstream orders
    // produce the same payload. Ties (missing or duplicate names) fall back
    // to canonical JSON so the result is fully deterministic.
    merged_array.sort_by(|a, b| {
        let a_name = a.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let b_name = b.get("name").and_then(|v| v.as_str()).unwrap_or("");
        a_name
            .cmp(b_name)
            .then_with(|| a.to_string().cmp(&b.to_string()))
    });

    Some(serde_json::json!({ array_key: merged_array }))
}

/// Project a list payload to its content-defining fields for hashing.
///
/// Hashing the full stored payload causes phantom version bumps when an
/// upstream rewords a description or adds optional metadata. This view
/// keeps only the identifier and the argument contract per item — the
/// fields that actually define the tool/resource/prompt API — so
/// version numbers track real API changes.
///
/// Per-method projection:
/// - `tools/list`              → `name`, `inputSchema`
/// - `prompts/list`            → `name`, `arguments`
/// - `resources/list`          → `name`, `uri`
/// - `resources/templates/list`→ `name`, `uriTemplate`
///
/// For non-list methods (e.g., `initialize`), returns the payload as-is.
pub fn canonical_hash_view(method: &str, payload: &Value) -> Value {
    let Some(array_key) = method_array_key(method) else {
        return payload.clone();
    };
    let keys: &[&str] = match method {
        "tools/list" => &["name", "inputSchema"],
        "prompts/list" => &["name", "arguments"],
        "resources/list" => &["name", "uri"],
        "resources/templates/list" => &["name", "uriTemplate"],
        _ => return payload.clone(),
    };
    let projected: Vec<Value> = payload
        .get(array_key)
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter().map(|item| project_keys(item, keys)).collect())
        .unwrap_or_default();
    serde_json::json!({ array_key: projected })
}

fn project_keys(item: &Value, keys: &[&str]) -> Value {
    let mut obj = serde_json::Map::new();
    for k in keys {
        if let Some(v) = item.get(*k) {
            obj.insert((*k).to_string(), v.clone());
        }
    }
    Value::Object(obj)
}

/// Diff two schema payloads for a list method.
///
/// Compares named items (by their `name` field) and returns granular
/// changes: added, removed, and modified items.
///
/// For methods without named items (e.g., `initialize`), returns a
/// single "updated" diff if the payloads differ.
pub fn diff_schema(method: &str, old_payload: &Value, new_payload: &Value) -> Vec<SchemaDiff> {
    let array_key = match method_array_key(method) {
        Some(key) => key,
        None => {
            // Non-list method (e.g., initialize) — no granular diff.
            return vec![SchemaDiff {
                change_type: "updated".to_string(),
                item_name: None,
            }];
        }
    };

    let item_type = method_item_type(method);
    let old_items = extract_named_items(old_payload, array_key);
    let new_items = extract_named_items(new_payload, array_key);

    let mut changes = Vec::new();

    // Find added and modified items.
    for (name, new_val) in &new_items {
        match old_items.get(name) {
            None => changes.push(SchemaDiff {
                change_type: format!("{item_type}_added"),
                item_name: Some(name.clone()),
            }),
            Some(old_val) if old_val != new_val => changes.push(SchemaDiff {
                change_type: format!("{item_type}_modified"),
                item_name: Some(name.clone()),
            }),
            _ => {} // unchanged
        }
    }

    // Find removed items.
    for name in old_items.keys() {
        if !new_items.contains_key(name) {
            changes.push(SchemaDiff {
                change_type: format!("{item_type}_removed"),
                item_name: Some(name.clone()),
            });
        }
    }

    if changes.is_empty() {
        // Hash changed but no named items differ — structural change.
        changes.push(SchemaDiff {
            change_type: "updated".to_string(),
            item_name: None,
        });
    }

    changes
}

// ── Internal helpers ─────────────────────────────────────────────────

/// Map an MCP list method to the array key in its `result` payload.
fn method_array_key(method: &str) -> Option<&'static str> {
    match method {
        "tools/list" => Some("tools"),
        "resources/list" => Some("resources"),
        "resources/templates/list" => Some("resourceTemplates"),
        "prompts/list" => Some("prompts"),
        _ => None,
    }
}

/// Map an MCP list method to a human-readable item type label used in
/// change records (e.g., "tool_added", "resource_removed").
fn method_item_type(method: &str) -> &'static str {
    match method {
        "tools/list" => "tool",
        "resources/list" => "resource",
        "resources/templates/list" => "resource_template",
        "prompts/list" => "prompt",
        _ => "item",
    }
}

/// Extract named items from a list payload as a map of name → JSON string.
///
/// MCP list items (tools, resources, prompts) have a `name` field that
/// serves as a stable identifier for diffing.
fn extract_named_items(payload: &Value, array_key: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    if let Some(arr) = payload.get(array_key).and_then(|a| a.as_array()) {
        for item in arr {
            if let Some(name) = item.get("name").and_then(|n| n.as_str()) {
                map.insert(name.to_string(), item.to_string());
            }
        }
    }
    map
}

// ── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── is_schema_method ─────────────────────────────────────────────

    #[test]
    fn is_schema_method__matches_discovery() {
        assert!(is_schema_method(&ClientMethod::Lifecycle(
            LifecycleMethod::Initialize
        )));
        assert!(is_schema_method(&ClientMethod::Tools(ToolsMethod::List)));
        assert!(is_schema_method(&ClientMethod::Resources(
            ResourcesMethod::List
        )));
        assert!(is_schema_method(&ClientMethod::Resources(
            ResourcesMethod::TemplatesList
        )));
        assert!(is_schema_method(&ClientMethod::Prompts(
            PromptsMethod::List
        )));
    }

    #[test]
    fn is_schema_method__rejects_non_discovery() {
        assert!(!is_schema_method(&ClientMethod::Tools(ToolsMethod::Call)));
        assert!(!is_schema_method(&ClientMethod::Resources(
            ResourcesMethod::Read
        )));
        assert!(!is_schema_method(&ClientMethod::Prompts(
            PromptsMethod::Get
        )));
        assert!(!is_schema_method(&ClientMethod::Ping));
        // Notifications have a separate enum; is_schema_method only
        // accepts ClientMethod (request-side), so they can't even be
        // constructed here. That's the type-level guarantee.
    }

    // ── detect_page_status ───────────────────────────────────────────

    #[test]
    fn detect_page_status__complete() {
        let req = json!({"method": "tools/list"});
        let resp = json!({"result": {"tools": []}});
        assert_eq!(detect_page_status(&req, &resp), PageStatus::Complete);
    }

    #[test]
    fn detect_page_status__first_page() {
        let req = json!({"method": "tools/list"});
        let resp = json!({"result": {"tools": [], "nextCursor": "abc"}});
        assert_eq!(detect_page_status(&req, &resp), PageStatus::FirstPage);
    }

    #[test]
    fn detect_page_status__middle_page() {
        let req = json!({"method": "tools/list", "params": {"cursor": "abc"}});
        let resp = json!({"result": {"tools": [], "nextCursor": "def"}});
        assert_eq!(detect_page_status(&req, &resp), PageStatus::MiddlePage);
    }

    #[test]
    fn detect_page_status__last_page() {
        let req = json!({"method": "tools/list", "params": {"cursor": "abc"}});
        let resp = json!({"result": {"tools": []}});
        assert_eq!(detect_page_status(&req, &resp), PageStatus::LastPage);
    }

    // ── merge_pages ──────────────────────────────────────────────────

    #[test]
    fn merge_pages__single() {
        let page = json!({"tools": [{"name": "a"}]});
        let result = merge_pages("tools/list", std::slice::from_ref(&page));
        assert_eq!(result, Some(page));
    }

    #[test]
    fn merge_pages__two_pages() {
        let p1 = json!({"tools": [{"name": "a"}]});
        let p2 = json!({"tools": [{"name": "b"}]});
        let result = merge_pages("tools/list", &[p1, p2]).unwrap();
        let tools = result["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0]["name"], "a");
        assert_eq!(tools[1]["name"], "b");
    }

    #[test]
    fn merge_pages__resources() {
        let p1 = json!({"resources": [{"name": "r1", "uri": "file://a"}]});
        let p2 = json!({"resources": [{"name": "r2", "uri": "file://b"}]});
        let result = merge_pages("resources/list", &[p1, p2]).unwrap();
        assert_eq!(result["resources"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn merge_pages__empty() {
        let result = merge_pages("tools/list", &[]);
        assert_eq!(result, None);
    }

    #[test]
    fn merge_pages__single_strips_volatile_metadata() {
        // Regression: Study Kit upstream returned 38 tools but produced 138
        // schema versions because the single-page branch kept the whole
        // raw result, including `_meta` / `serverInfo` fields that the
        // server regenerates per request.
        let p1 = json!({
            "tools": [{"name": "a"}],
            "_meta": {"requestId": "req-1"},
            "serverInfo": {"generatedAt": "2026-04-19T00:00:00Z"}
        });
        let p2 = json!({
            "tools": [{"name": "a"}],
            "_meta": {"requestId": "req-2"},
            "serverInfo": {"generatedAt": "2026-04-19T00:00:05Z"}
        });
        let r1 = merge_pages("tools/list", &[p1]).unwrap();
        let r2 = merge_pages("tools/list", &[p2]).unwrap();
        assert_eq!(r1, r2, "per-request metadata must not reach the hash");
        assert_eq!(r1, json!({"tools": [{"name": "a"}]}));
    }

    #[test]
    fn merge_pages__single_missing_array_key_yields_empty_array() {
        let p1 = json!({"_meta": {"requestId": "x"}});
        let result = merge_pages("tools/list", &[p1]).unwrap();
        assert_eq!(result, json!({"tools": []}));
    }

    #[test]
    fn merge_pages__unknown_method_single_returns_as_is() {
        let p1 = json!({"serverInfo": {"name": "test"}});
        let result = merge_pages("initialize", std::slice::from_ref(&p1));
        assert_eq!(result, Some(p1));
    }

    #[test]
    fn merge_pages__unknown_method_multi_returns_none() {
        let p1 = json!({"serverInfo": {"name": "v1"}});
        let p2 = json!({"serverInfo": {"name": "v2"}});
        let result = merge_pages("initialize", &[p1, p2]);
        assert_eq!(result, None);
    }

    #[test]
    fn merge_pages__sorts_items_by_name() {
        let page = json!({"tools": [
            {"name": "c"}, {"name": "a"}, {"name": "b"}
        ]});
        let result = merge_pages("tools/list", &[page]).unwrap();
        let names: Vec<&str> = result["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["a", "b", "c"]);
    }

    #[test]
    fn merge_pages__same_set_in_different_orders_is_equal() {
        // Regression: upstream returning identical tool sets in different
        // orders previously produced different hashes and inflated the
        // version count.
        let p1 = json!({"tools": [{"name": "a"}, {"name": "b"}, {"name": "c"}]});
        let p2 = json!({"tools": [{"name": "c"}, {"name": "a"}, {"name": "b"}]});
        assert_eq!(
            merge_pages("tools/list", &[p1]),
            merge_pages("tools/list", &[p2]),
        );
    }

    #[test]
    fn merge_pages__preserves_description_for_display() {
        let page = json!({"tools": [
            {"name": "a", "description": "do thing", "inputSchema": {"type": "object"}}
        ]});
        let result = merge_pages("tools/list", &[page]).unwrap();
        assert_eq!(result["tools"][0]["description"], "do thing");
    }

    #[test]
    fn merge_pages__items_without_name_break_ties_by_canonical_json() {
        let p1 = json!({"tools": [{"foo": "1"}, {"foo": "2"}]});
        let p2 = json!({"tools": [{"foo": "2"}, {"foo": "1"}]});
        assert_eq!(
            merge_pages("tools/list", &[p1]),
            merge_pages("tools/list", &[p2]),
        );
    }

    // ── canonical_hash_view ──────────────────────────────────────────

    #[test]
    fn canonical_hash_view__tool_keeps_only_name_and_input_schema() {
        let payload = json!({"tools": [{
            "name": "search",
            "description": "human text",
            "inputSchema": {"type": "object", "properties": {"q": {"type": "string"}}},
            "annotations": {"readOnlyHint": true}
        }]});
        let view = canonical_hash_view("tools/list", &payload);
        assert_eq!(
            view,
            json!({"tools": [{
                "name": "search",
                "inputSchema": {"type": "object", "properties": {"q": {"type": "string"}}}
            }]}),
        );
    }

    #[test]
    fn canonical_hash_view__resource_keeps_only_name_and_uri() {
        let payload = json!({"resources": [{
            "name": "r1",
            "uri": "file://a",
            "description": "desc",
            "mimeType": "text/plain"
        }]});
        let view = canonical_hash_view("resources/list", &payload);
        assert_eq!(
            view,
            json!({"resources": [{"name": "r1", "uri": "file://a"}]}),
        );
    }

    #[test]
    fn canonical_hash_view__prompt_keeps_only_name_and_arguments() {
        let payload = json!({"prompts": [{
            "name": "summarize",
            "description": "summarizes text",
            "arguments": [{"name": "topic", "required": true}]
        }]});
        let view = canonical_hash_view("prompts/list", &payload);
        assert_eq!(
            view,
            json!({"prompts": [{
                "name": "summarize",
                "arguments": [{"name": "topic", "required": true}]
            }]}),
        );
    }

    #[test]
    fn canonical_hash_view__resource_template_keeps_name_and_uri_template() {
        let payload = json!({"resourceTemplates": [{
            "name": "doc",
            "uriTemplate": "doc://{id}",
            "description": "any doc",
            "mimeType": "text/markdown"
        }]});
        let view = canonical_hash_view("resources/templates/list", &payload);
        assert_eq!(
            view,
            json!({"resourceTemplates": [{"name": "doc", "uriTemplate": "doc://{id}"}]}),
        );
    }

    #[test]
    fn canonical_hash_view__description_only_change_is_invisible() {
        let p1 = json!({"tools": [{"name": "a", "description": "old", "inputSchema": {}}]});
        let p2 = json!({"tools": [{"name": "a", "description": "new", "inputSchema": {}}]});
        assert_eq!(
            canonical_hash_view("tools/list", &p1),
            canonical_hash_view("tools/list", &p2),
        );
    }

    #[test]
    fn canonical_hash_view__input_schema_change_is_visible() {
        let p1 = json!({"tools": [{"name": "a", "inputSchema": {"type": "object"}}]});
        let p2 = json!({"tools": [{
            "name": "a",
            "inputSchema": {"type": "object", "properties": {"q": {"type": "string"}}}
        }]});
        assert_ne!(
            canonical_hash_view("tools/list", &p1),
            canonical_hash_view("tools/list", &p2),
        );
    }

    #[test]
    fn canonical_hash_view__non_list_method_returns_unchanged() {
        let payload = json!({"serverInfo": {"name": "test", "version": "1.0"}});
        assert_eq!(canonical_hash_view("initialize", &payload), payload);
    }

    #[test]
    fn canonical_hash_view__missing_array_key_yields_empty_array() {
        let payload = json!({"_meta": {"requestId": "x"}});
        assert_eq!(
            canonical_hash_view("tools/list", &payload),
            json!({"tools": []}),
        );
    }

    // ── diff_schema ──────────────────────────────────────────────────

    #[test]
    fn diff_schema__tool_added() {
        let old = json!({"tools": [{"name": "a", "description": "tool a"}]});
        let new = json!({"tools": [
            {"name": "a", "description": "tool a"},
            {"name": "b", "description": "tool b"}
        ]});
        let diffs = diff_schema("tools/list", &old, &new);
        assert_eq!(diffs.len(), 1);
        assert_eq!(diffs[0].change_type, "tool_added");
        assert_eq!(diffs[0].item_name.as_deref(), Some("b"));
    }

    #[test]
    fn diff_schema__tool_removed() {
        let old = json!({"tools": [
            {"name": "a", "description": "tool a"},
            {"name": "b", "description": "tool b"}
        ]});
        let new = json!({"tools": [{"name": "a", "description": "tool a"}]});
        let diffs = diff_schema("tools/list", &old, &new);
        assert_eq!(diffs.len(), 1);
        assert_eq!(diffs[0].change_type, "tool_removed");
        assert_eq!(diffs[0].item_name.as_deref(), Some("b"));
    }

    #[test]
    fn diff_schema__tool_modified() {
        let old = json!({"tools": [{"name": "a", "description": "old desc"}]});
        let new = json!({"tools": [{"name": "a", "description": "new desc"}]});
        let diffs = diff_schema("tools/list", &old, &new);
        assert_eq!(diffs.len(), 1);
        assert_eq!(diffs[0].change_type, "tool_modified");
        assert_eq!(diffs[0].item_name.as_deref(), Some("a"));
    }

    #[test]
    fn diff_schema__no_change() {
        let payload = json!({"tools": [{"name": "a", "description": "tool a"}]});
        let diffs = diff_schema("tools/list", &payload, &payload);
        assert_eq!(diffs.len(), 1);
        assert_eq!(diffs[0].change_type, "updated");
        assert_eq!(diffs[0].item_name, None);
    }

    #[test]
    fn diff_schema__multiple_changes() {
        let old = json!({"tools": [
            {"name": "a", "description": "old a"},
            {"name": "b", "description": "tool b"}
        ]});
        let new = json!({"tools": [
            {"name": "a", "description": "new a"},
            {"name": "c", "description": "tool c"}
        ]});
        let diffs = diff_schema("tools/list", &old, &new);
        let types: Vec<&str> = diffs.iter().map(|d| d.change_type.as_str()).collect();
        assert!(types.contains(&"tool_modified")); // a modified
        assert!(types.contains(&"tool_added")); // c added
        assert!(types.contains(&"tool_removed")); // b removed
        assert_eq!(diffs.len(), 3);
    }

    #[test]
    fn diff_schema__initialize_returns_updated() {
        let old = json!({"serverInfo": {"name": "test", "version": "1.0"}});
        let new = json!({"serverInfo": {"name": "test", "version": "2.0"}});
        let diffs = diff_schema("initialize", &old, &new);
        assert_eq!(diffs.len(), 1);
        assert_eq!(diffs[0].change_type, "updated");
        assert_eq!(diffs[0].item_name, None);
    }

    #[test]
    fn diff_schema__prompts() {
        let old = json!({"prompts": [{"name": "summarize"}]});
        let new = json!({"prompts": [{"name": "summarize"}, {"name": "translate"}]});
        let diffs = diff_schema("prompts/list", &old, &new);
        assert_eq!(diffs.len(), 1);
        assert_eq!(diffs[0].change_type, "prompt_added");
        assert_eq!(diffs[0].item_name.as_deref(), Some("translate"));
    }

    #[test]
    fn diff_schema__resources() {
        let old = json!({"resources": [
            {"name": "file1", "uri": "file://a"},
            {"name": "file2", "uri": "file://b"}
        ]});
        let new = json!({"resources": [{"name": "file1", "uri": "file://a"}]});
        let diffs = diff_schema("resources/list", &old, &new);
        assert_eq!(diffs.len(), 1);
        assert_eq!(diffs[0].change_type, "resource_removed");
        assert_eq!(diffs[0].item_name.as_deref(), Some("file2"));
    }

    // ── method_array_key ─────────────────────────────────────────────

    #[test]
    fn method_array_key__mapping() {
        assert_eq!(method_array_key("tools/list"), Some("tools"));
        assert_eq!(method_array_key("resources/list"), Some("resources"));
        assert_eq!(
            method_array_key("resources/templates/list"),
            Some("resourceTemplates")
        );
        assert_eq!(method_array_key("prompts/list"), Some("prompts"));
        assert_eq!(method_array_key("initialize"), None);
        assert_eq!(method_array_key("tools/call"), None);
    }
}
