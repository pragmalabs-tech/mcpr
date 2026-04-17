//! # Response rewriting for widget CSP
//!
//! Mutates JSON-RPC response bodies so widgets see the right CSP when hosts
//! render them. Two things happen on every rewrite:
//!
//! 1. Widget metadata (`_meta.openai/widgetCSP` and `_meta.ui.csp`) is rebuilt
//!    per directive using [`super::csp::effective_domains`]. The merge knows
//!    about the per-directive mode, the widget-scoped overrides, and the
//!    proxy URL that always prepends the list.
//! 2. A conservative deep scan walks the entire response and inserts the proxy
//!    URL into any CSP domain array it finds. This catches servers that embed
//!    CSP arrays in non-standard locations.
//!
//! The rewrite never touches response body `text` or `blob` fields — widget
//! HTML is served verbatim.

use serde_json::Value;

use super::csp::{CspConfig, Directive, effective_domains};
use crate::protocol as jsonrpc;

/// Runtime configuration for response rewriting.
#[derive(Clone)]
pub struct RewriteConfig {
    /// Proxy URL (scheme + host, no trailing slash) to insert into every CSP
    /// array so widgets can reach the proxy.
    pub proxy_url: String,
    /// Bare proxy host (no scheme) used when rewriting `widgetDomain`.
    pub proxy_domain: String,
    /// Upstream MCP URL, used to recognise and strip upstream self-references
    /// from the CSP arrays the server returns.
    pub mcp_upstream: String,
    /// Declarative CSP config — global policies plus widget-scoped overrides.
    pub csp: CspConfig,
}

/// Rewrite a JSON-RPC response in place for the given method.
pub fn rewrite_response(method: &str, body: &mut Value, config: &RewriteConfig) {
    match method {
        jsonrpc::TOOLS_LIST => {
            if let Some(tools) = body
                .get_mut("result")
                .and_then(|r| r.get_mut("tools"))
                .and_then(|t| t.as_array_mut())
            {
                for tool in tools {
                    if let Some(meta) = tool.get_mut("meta") {
                        rewrite_widget_meta(meta, None, config);
                    }
                }
            }
        }
        jsonrpc::TOOLS_CALL => {
            if let Some(meta) = body.get_mut("result").and_then(|r| r.get_mut("meta")) {
                rewrite_widget_meta(meta, None, config);
            }
        }
        jsonrpc::RESOURCES_LIST => {
            if let Some(resources) = body
                .get_mut("result")
                .and_then(|r| r.get_mut("resources"))
                .and_then(|r| r.as_array_mut())
            {
                for resource in resources {
                    let uri = resource
                        .get("uri")
                        .and_then(|v| v.as_str())
                        .map(String::from);
                    if let Some(meta) = resource.get_mut("meta") {
                        rewrite_widget_meta(meta, uri.as_deref(), config);
                    }
                }
            }
        }
        jsonrpc::RESOURCES_TEMPLATES_LIST => {
            if let Some(templates) = body
                .get_mut("result")
                .and_then(|r| r.get_mut("resourceTemplates"))
                .and_then(|t| t.as_array_mut())
            {
                for template in templates {
                    // Templates expose `uriTemplate`, not a concrete URI; treat
                    // it as the match key so operators can glob on template IDs.
                    let uri = template
                        .get("uriTemplate")
                        .and_then(|v| v.as_str())
                        .map(String::from);
                    if let Some(meta) = template.get_mut("meta") {
                        rewrite_widget_meta(meta, uri.as_deref(), config);
                    }
                }
            }
        }
        jsonrpc::RESOURCES_READ => {
            if let Some(contents) = body
                .get_mut("result")
                .and_then(|r| r.get_mut("contents"))
                .and_then(|c| c.as_array_mut())
            {
                for content in contents {
                    let uri = content
                        .get("uri")
                        .and_then(|v| v.as_str())
                        .map(String::from);
                    if let Some(meta) = content.get_mut("meta") {
                        rewrite_widget_meta(meta, uri.as_deref(), config);
                    }
                }
            }
        }
        _ => {}
    }

    // Safety net: any CSP-shaped domain array elsewhere in the tree still gets
    // the proxy URL. The merge rules above do not run here — this pass only
    // guarantees the proxy URL is present.
    inject_proxy_into_all_csp(body, config);
}

/// Rewrite a widget metadata object. Handles both the OpenAI (`openai/widgetCSP`,
/// `openai/widgetDomain`) and Claude/spec (`ui.csp`, `ui.domain`) shapes.
///
/// `explicit_uri` is the resource URI the caller already resolved (for example,
/// a `resources/read` caller knows the URI from the containing object). When
/// missing, the URI is inferred from `meta.ui.resourceUri` or the legacy
/// `openai/outputTemplate` field.
fn rewrite_widget_meta(meta: &mut Value, explicit_uri: Option<&str>, config: &RewriteConfig) {
    if meta.get("openai/widgetDomain").is_some() {
        meta["openai/widgetDomain"] = Value::String(config.proxy_domain.clone());
    }

    let inferred = explicit_uri
        .map(String::from)
        .or_else(|| extract_resource_uri(meta));
    let uri = inferred.as_deref();

    // OpenAI legacy shape uses snake_case keys.
    rewrite_directive(
        meta,
        "openai/widgetCSP",
        "connect_domains",
        Directive::Connect,
        uri,
        config,
    );
    rewrite_directive(
        meta,
        "openai/widgetCSP",
        "resource_domains",
        Directive::Resource,
        uri,
        config,
    );
    rewrite_directive(
        meta,
        "openai/widgetCSP",
        "frame_domains",
        Directive::Frame,
        uri,
        config,
    );

    // Spec / Claude shape uses camelCase keys under `ui.csp`.
    if let Some(ui) = meta.get_mut("ui") {
        rewrite_directive(ui, "csp", "connectDomains", Directive::Connect, uri, config);
        rewrite_directive(
            ui,
            "csp",
            "resourceDomains",
            Directive::Resource,
            uri,
            config,
        );
        rewrite_directive(ui, "csp", "frameDomains", Directive::Frame, uri, config);
    }

    inject_proxy_into_all_csp(meta, config);
}

/// Extract a resource URI from a widget meta object. Prefers the spec field
/// (`ui.resourceUri`) and falls back to the OpenAI legacy key.
fn extract_resource_uri(meta: &Value) -> Option<String> {
    if let Some(u) = meta.pointer("/ui/resourceUri").and_then(|v| v.as_str()) {
        return Some(u.to_string());
    }
    meta.get("openai/outputTemplate")
        .and_then(|v| v.as_str())
        .map(String::from)
}

/// Recompute one CSP directive array in place using [`effective_domains`].
/// Skips silently when the parent object or the directive array is absent.
fn rewrite_directive(
    parent: &mut Value,
    obj_key: &str,
    array_key: &str,
    directive: Directive,
    resource_uri: Option<&str>,
    config: &RewriteConfig,
) {
    let Some(obj) = parent.get_mut(obj_key) else {
        return;
    };
    let Some(arr) = obj.get_mut(array_key).and_then(|v| v.as_array_mut()) else {
        return;
    };

    let upstream: Vec<String> = arr
        .iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect();

    let upstream_host = strip_scheme(&config.mcp_upstream);

    let merged = effective_domains(
        &config.csp,
        directive,
        resource_uri,
        &upstream,
        &upstream_host,
        &config.proxy_url,
    );

    *obj.get_mut(array_key).unwrap() =
        Value::Array(merged.into_iter().map(Value::String).collect());
}

/// Recursively ensure the proxy URL is present in every CSP-shaped domain array.
/// Does not apply the merge rules — that would require URI context the deep scan
/// does not have. The only guarantee is "proxy URL is reachable from the widget."
fn inject_proxy_into_all_csp(value: &mut Value, config: &RewriteConfig) {
    match value {
        Value::Object(map) => {
            for key in [
                "connect_domains",
                "resource_domains",
                "frame_domains",
                "connectDomains",
                "resourceDomains",
                "frameDomains",
            ] {
                if let Some(arr) = map.get_mut(key).and_then(|v| v.as_array_mut()) {
                    let has_proxy = arr.iter().any(|v| v.as_str() == Some(&config.proxy_url));
                    if !has_proxy {
                        arr.insert(0, Value::String(config.proxy_url.clone()));
                    }
                }
            }
            for (_, v) in map.iter_mut() {
                inject_proxy_into_all_csp(v, config);
            }
        }
        Value::Array(arr) => {
            for item in arr {
                inject_proxy_into_all_csp(item, config);
            }
        }
        _ => {}
    }
}

fn strip_scheme(url: &str) -> String {
    url.trim_start_matches("https://")
        .trim_start_matches("http://")
        .split('/')
        .next()
        .unwrap_or("")
        .to_string()
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;
    use crate::proxy::csp::{DirectivePolicy, Mode, WidgetScoped};
    use serde_json::json;

    // ── helpers ────────────────────────────────────────────────────────────

    fn rewrite_config() -> RewriteConfig {
        RewriteConfig {
            proxy_url: "https://abc.tunnel.example.com".into(),
            proxy_domain: "abc.tunnel.example.com".into(),
            mcp_upstream: "http://localhost:9000".into(),
            csp: CspConfig::default(),
        }
    }

    fn as_strs(arr: &Value) -> Vec<&str> {
        arr.as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect()
    }

    // ── resources/read: HTML body is never touched ─────────────────────────

    #[test]
    fn rewrite_response__resources_read_preserves_html() {
        let config = rewrite_config();
        let mut body = json!({
            "jsonrpc": "2.0", "id": 1,
            "result": {
                "contents": [{
                    "uri": "ui://widget/question",
                    "mimeType": "text/html",
                    "text": "<html><script src=\"/assets/main.js\"></script></html>"
                }]
            }
        });
        let original = body["result"]["contents"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();

        rewrite_response("resources/read", &mut body, &config);

        assert_eq!(
            body["result"]["contents"][0]["text"].as_str().unwrap(),
            original
        );
    }

    // ── resources/read: rewrites meta, not text ────────────────────────────

    #[test]
    fn rewrite_response__resources_read_rewrites_meta_not_text() {
        let config = rewrite_config();
        let mut body = json!({
            "result": {
                "contents": [{
                    "uri": "ui://widget/question",
                    "mimeType": "text/html",
                    "text": "<html><body>Hello</body></html>",
                    "meta": {
                        "openai/widgetDomain": "localhost:9000",
                        "openai/widgetCSP": {
                            "resource_domains": ["http://localhost:9000"],
                            "connect_domains": ["http://localhost:9000"]
                        }
                    }
                }]
            }
        });

        rewrite_response("resources/read", &mut body, &config);

        let content = &body["result"]["contents"][0];
        assert_eq!(
            content["text"].as_str().unwrap(),
            "<html><body>Hello</body></html>"
        );
        assert_eq!(
            content["meta"]["openai/widgetDomain"].as_str().unwrap(),
            "abc.tunnel.example.com"
        );
        let resources = as_strs(&content["meta"]["openai/widgetCSP"]["resource_domains"]);
        assert!(resources.contains(&"https://abc.tunnel.example.com"));
        assert!(!resources.iter().any(|d| d.contains("localhost")));
    }

    // ── tools/list: per-tool meta rewrite ──────────────────────────────────

    #[test]
    fn rewrite_response__tools_list_rewrites_widget_domain() {
        let config = rewrite_config();
        let mut body = json!({
            "result": {
                "tools": [{
                    "name": "create_question",
                    "meta": {
                        "openai/widgetDomain": "old.domain.com",
                        "openai/widgetCSP": {
                            "resource_domains": ["http://localhost:4444"],
                            "connect_domains": ["http://localhost:9000", "https://api.external.com"]
                        }
                    }
                }]
            }
        });

        rewrite_response("tools/list", &mut body, &config);

        let meta = &body["result"]["tools"][0]["meta"];
        assert_eq!(
            meta["openai/widgetDomain"].as_str().unwrap(),
            "abc.tunnel.example.com"
        );
        let connect = as_strs(&meta["openai/widgetCSP"]["connect_domains"]);
        assert!(connect.contains(&"https://abc.tunnel.example.com"));
        assert!(connect.contains(&"https://api.external.com"));
        assert!(!connect.iter().any(|d| d.contains("localhost")));
    }

    // ── tools/call: rewrites result.meta ───────────────────────────────────

    #[test]
    fn rewrite_response__tools_call_rewrites_meta() {
        let config = rewrite_config();
        let mut body = json!({
            "result": {
                "content": [{"type": "text", "text": "some result"}],
                "meta": {
                    "openai/widgetDomain": "old.domain.com",
                    "openai/widgetCSP": {
                        "resource_domains": ["http://localhost:4444"]
                    }
                }
            }
        });

        rewrite_response("tools/call", &mut body, &config);

        assert_eq!(
            body["result"]["meta"]["openai/widgetDomain"]
                .as_str()
                .unwrap(),
            "abc.tunnel.example.com"
        );
        assert_eq!(
            body["result"]["content"][0]["text"].as_str().unwrap(),
            "some result"
        );
    }

    // ── resources/list ─────────────────────────────────────────────────────

    #[test]
    fn rewrite_response__resources_list_rewrites_meta() {
        let config = rewrite_config();
        let mut body = json!({
            "result": {
                "resources": [{
                    "uri": "ui://widget/question",
                    "name": "Question Widget",
                    "meta": {
                        "openai/widgetDomain": "old.domain.com"
                    }
                }]
            }
        });

        rewrite_response("resources/list", &mut body, &config);

        assert_eq!(
            body["result"]["resources"][0]["meta"]["openai/widgetDomain"]
                .as_str()
                .unwrap(),
            "abc.tunnel.example.com"
        );
    }

    // ── resources/templates/list ───────────────────────────────────────────

    #[test]
    fn rewrite_response__resources_templates_list_rewrites_meta() {
        let config = rewrite_config();
        let mut body = json!({
            "result": {
                "resourceTemplates": [{
                    "uriTemplate": "file:///{path}",
                    "name": "File Access",
                    "meta": {
                        "openai/widgetDomain": "old.domain.com",
                        "openai/widgetCSP": {
                            "resource_domains": ["http://localhost:4444"],
                            "connect_domains": ["http://localhost:9000"]
                        }
                    }
                }]
            }
        });

        rewrite_response("resources/templates/list", &mut body, &config);

        let meta = &body["result"]["resourceTemplates"][0]["meta"];
        assert_eq!(
            meta["openai/widgetDomain"].as_str().unwrap(),
            "abc.tunnel.example.com"
        );
        let resources = as_strs(&meta["openai/widgetCSP"]["resource_domains"]);
        assert!(resources.contains(&"https://abc.tunnel.example.com"));
        assert!(!resources.iter().any(|d| d.contains("localhost")));
    }

    // ── CSP merge: localhost stripping ─────────────────────────────────────

    #[test]
    fn rewrite_response__csp_strips_localhost() {
        let config = rewrite_config();
        let mut body = json!({
            "result": {
                "tools": [{
                    "name": "test",
                    "meta": {
                        "openai/widgetCSP": {
                            "resource_domains": [
                                "http://localhost:4444",
                                "http://127.0.0.1:4444",
                                "http://localhost:9000",
                                "https://cdn.external.com"
                            ]
                        }
                    }
                }]
            }
        });

        rewrite_response("tools/list", &mut body, &config);

        let domains =
            as_strs(&body["result"]["tools"][0]["meta"]["openai/widgetCSP"]["resource_domains"]);
        assert_eq!(
            domains,
            vec!["https://abc.tunnel.example.com", "https://cdn.external.com"]
        );
    }

    // ── CSP merge: declared global domains are appended ────────────────────

    #[test]
    fn rewrite_response__global_connect_domains_appended() {
        let mut config = rewrite_config();
        config.csp.connect_domains = DirectivePolicy {
            domains: vec!["https://extra.example.com".into()],
            mode: Mode::Extend,
        };

        let mut body = json!({
            "result": {
                "tools": [{
                    "name": "test",
                    "meta": {
                        "openai/widgetCSP": {
                            "connect_domains": ["http://localhost:9000"]
                        }
                    }
                }]
            }
        });

        rewrite_response("tools/list", &mut body, &config);

        let domains =
            as_strs(&body["result"]["tools"][0]["meta"]["openai/widgetCSP"]["connect_domains"]);
        assert!(domains.contains(&"https://extra.example.com"));
        assert!(domains.contains(&"https://abc.tunnel.example.com"));
    }

    // ── CSP merge: no duplicate proxy entries ──────────────────────────────

    #[test]
    fn rewrite_response__csp_no_duplicate_proxy() {
        let config = rewrite_config();
        let mut body = json!({
            "result": {
                "tools": [{
                    "name": "test",
                    "meta": {
                        "openai/widgetCSP": {
                            "resource_domains": ["https://abc.tunnel.example.com", "https://cdn.example.com"]
                        }
                    }
                }]
            }
        });

        rewrite_response("tools/list", &mut body, &config);

        let domains =
            as_strs(&body["result"]["tools"][0]["meta"]["openai/widgetCSP"]["resource_domains"]);
        let count = domains
            .iter()
            .filter(|d| **d == "https://abc.tunnel.example.com")
            .count();
        assert_eq!(count, 1);
    }

    // ── Claude format parity ───────────────────────────────────────────────

    #[test]
    fn rewrite_response__claude_csp_format() {
        let config = rewrite_config();
        let mut body = json!({
            "result": {
                "tools": [{
                    "name": "test",
                    "meta": {
                        "ui": {
                            "csp": {
                                "connectDomains": ["http://localhost:9000"],
                                "resourceDomains": ["http://localhost:4444"]
                            }
                        }
                    }
                }]
            }
        });

        rewrite_response("tools/list", &mut body, &config);

        let meta = &body["result"]["tools"][0]["meta"]["ui"]["csp"];
        let connect = as_strs(&meta["connectDomains"]);
        let resource = as_strs(&meta["resourceDomains"]);
        assert!(connect.contains(&"https://abc.tunnel.example.com"));
        assert!(resource.contains(&"https://abc.tunnel.example.com"));
        assert!(!connect.iter().any(|d| d.contains("localhost")));
        assert!(!resource.iter().any(|d| d.contains("localhost")));
    }

    // ── Deep CSP injection fallback ────────────────────────────────────────

    #[test]
    fn rewrite_response__deep_csp_injection() {
        let config = rewrite_config();
        let mut body = json!({
            "result": {
                "content": [{
                    "type": "text",
                    "text": "result",
                    "deeply": {
                        "nested": {
                            "connect_domains": ["https://only-external.com"]
                        }
                    }
                }]
            }
        });

        rewrite_response("tools/call", &mut body, &config);

        let domains = as_strs(&body["result"]["content"][0]["deeply"]["nested"]["connect_domains"]);
        assert!(domains.contains(&"https://abc.tunnel.example.com"));
    }

    // ── Unknown methods: only deep scan runs ───────────────────────────────

    #[test]
    fn rewrite_response__unknown_method_passthrough() {
        let config = rewrite_config();
        let mut body = json!({
            "result": {
                "data": "unchanged",
                "meta": { "openai/widgetDomain": "should-stay.com" }
            }
        });
        rewrite_response("notifications/message", &mut body, &config);

        assert_eq!(
            body["result"]["meta"]["openai/widgetDomain"]
                .as_str()
                .unwrap(),
            "should-stay.com"
        );
        assert_eq!(body["result"]["data"].as_str().unwrap(), "unchanged");
    }

    // ── Global replace mode ───────────────────────────────────────────────

    #[test]
    fn rewrite_response__replace_mode_ignores_upstream() {
        let mut config = rewrite_config();
        config.csp.resource_domains = DirectivePolicy {
            domains: vec!["https://allowed.example.com".into()],
            mode: Mode::Replace,
        };
        config.csp.connect_domains = DirectivePolicy {
            domains: vec!["https://allowed.example.com".into()],
            mode: Mode::Replace,
        };

        let mut body = json!({
            "result": {
                "tools": [{
                    "name": "test",
                    "meta": {
                        "openai/widgetCSP": {
                            "resource_domains": ["https://cdn.external.com", "https://api.external.com"],
                            "connect_domains": ["https://api.external.com", "http://localhost:9000"]
                        }
                    }
                }]
            }
        });

        rewrite_response("tools/list", &mut body, &config);

        let resources =
            as_strs(&body["result"]["tools"][0]["meta"]["openai/widgetCSP"]["resource_domains"]);
        assert_eq!(
            resources,
            vec![
                "https://abc.tunnel.example.com",
                "https://allowed.example.com"
            ]
        );
        let connect =
            as_strs(&body["result"]["tools"][0]["meta"]["openai/widgetCSP"]["connect_domains"]);
        assert_eq!(
            connect,
            vec![
                "https://abc.tunnel.example.com",
                "https://allowed.example.com"
            ]
        );
    }

    // ── Widget-scoped overrides ────────────────────────────────────────────

    #[test]
    fn rewrite_response__widget_scope_matches_resource_uri() {
        // A widget override should only apply when the resource URI matches.
        let mut config = rewrite_config();
        config.csp.widgets.push(WidgetScoped {
            match_pattern: "ui://widget/payment*".into(),
            connect_domains: vec!["https://api.stripe.com".into()],
            connect_domains_mode: Mode::Extend,
            ..Default::default()
        });

        let mut body = json!({
            "result": {
                "resources": [
                    {
                        "uri": "ui://widget/payment-form",
                        "meta": {
                            "openai/widgetCSP": { "connect_domains": [] }
                        }
                    },
                    {
                        "uri": "ui://widget/search",
                        "meta": {
                            "openai/widgetCSP": { "connect_domains": [] }
                        }
                    }
                ]
            }
        });

        rewrite_response("resources/list", &mut body, &config);

        let payment_connect =
            as_strs(&body["result"]["resources"][0]["meta"]["openai/widgetCSP"]["connect_domains"]);
        assert!(payment_connect.contains(&"https://api.stripe.com"));

        let search_connect =
            as_strs(&body["result"]["resources"][1]["meta"]["openai/widgetCSP"]["connect_domains"]);
        assert!(!search_connect.contains(&"https://api.stripe.com"));
    }

    #[test]
    fn rewrite_response__widget_replace_mode_wipes_upstream() {
        let mut config = rewrite_config();
        config.csp.widgets.push(WidgetScoped {
            match_pattern: "ui://widget/*".into(),
            connect_domains: vec!["https://api.stripe.com".into()],
            connect_domains_mode: Mode::Replace,
            ..Default::default()
        });

        let mut body = json!({
            "result": {
                "contents": [{
                    "uri": "ui://widget/payment",
                    "meta": {
                        "openai/widgetCSP": {
                            "connect_domains": [
                                "https://api.external.com",
                                "https://another.external.com"
                            ]
                        }
                    }
                }]
            }
        });

        rewrite_response("resources/read", &mut body, &config);

        let connect =
            as_strs(&body["result"]["contents"][0]["meta"]["openai/widgetCSP"]["connect_domains"]);
        assert_eq!(
            connect,
            vec!["https://abc.tunnel.example.com", "https://api.stripe.com"]
        );
    }

    #[test]
    fn rewrite_response__widget_uri_inferred_from_tool_meta() {
        // tools/list responses do not carry a URI on the tool itself, but the
        // widget resource URI lives in meta.ui.resourceUri; widget overrides
        // should match against that.
        let mut config = rewrite_config();
        config.csp.widgets.push(WidgetScoped {
            match_pattern: "ui://widget/payment*".into(),
            connect_domains: vec!["https://api.stripe.com".into()],
            connect_domains_mode: Mode::Extend,
            ..Default::default()
        });

        let mut body = json!({
            "result": {
                "tools": [{
                    "name": "take_payment",
                    "meta": {
                        "ui": { "resourceUri": "ui://widget/payment-form" },
                        "openai/widgetCSP": { "connect_domains": [] }
                    }
                }]
            }
        });

        rewrite_response("tools/list", &mut body, &config);

        let connect =
            as_strs(&body["result"]["tools"][0]["meta"]["openai/widgetCSP"]["connect_domains"]);
        assert!(connect.contains(&"https://api.stripe.com"));
    }

    // ── Frame directive defaults strict ────────────────────────────────────

    #[test]
    fn rewrite_response__frame_domains_default_replace_drops_upstream() {
        // Default config treats frameDomains as replace, so upstream values are
        // dropped even in the absence of any declared frame domains.
        let config = rewrite_config();
        let mut body = json!({
            "result": {
                "tools": [{
                    "name": "test",
                    "meta": {
                        "ui": {
                            "csp": {
                                "frameDomains": ["https://embed.external.com"]
                            }
                        }
                    }
                }]
            }
        });

        rewrite_response("tools/list", &mut body, &config);

        let frames = as_strs(&body["result"]["tools"][0]["meta"]["ui"]["csp"]["frameDomains"]);
        assert_eq!(frames, vec!["https://abc.tunnel.example.com"]);
    }
}
