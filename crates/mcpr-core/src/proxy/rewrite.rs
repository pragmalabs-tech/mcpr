use serde_json::Value;

use super::csp::CspMode;
use crate::protocol as jsonrpc;

#[derive(Clone)]
pub struct RewriteConfig {
    pub proxy_url: String,
    pub proxy_domain: String,
    pub mcp_upstream: String,
    pub extra_csp_domains: Vec<String>,
    pub csp_mode: CspMode,
}

/// Rewrite a JSON-RPC response based on the method.
/// Phase 1: inject proxy URL into known metadata fields (no dynamic learning).
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
                        rewrite_widget_meta(meta, config);
                    }
                }
            }
        }
        jsonrpc::TOOLS_CALL => {
            // _meta is at result.meta in the JSON-RPC response
            if let Some(meta) = body.get_mut("result").and_then(|r| r.get_mut("meta")) {
                rewrite_widget_meta(meta, config);
            }
        }
        jsonrpc::RESOURCES_LIST => {
            if let Some(resources) = body
                .get_mut("result")
                .and_then(|r| r.get_mut("resources"))
                .and_then(|r| r.as_array_mut())
            {
                for resource in resources {
                    if let Some(meta) = resource.get_mut("meta") {
                        rewrite_widget_meta(meta, config);
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
                    if let Some(meta) = template.get_mut("meta") {
                        rewrite_widget_meta(meta, config);
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
                    if let Some(meta) = content.get_mut("meta") {
                        rewrite_widget_meta(meta, config);
                    }
                }
            }
        }
        _ => {} // passthrough
    }

    // Always do a deep scan on the entire response to catch any CSP arrays we missed
    inject_proxy_into_all_csp(body, config);
}

/// Rewrite widget metadata: domain, CSP arrays (both OpenAI and Claude formats).
fn rewrite_widget_meta(meta: &mut Value, config: &RewriteConfig) {
    // openai/widgetDomain → proxy domain
    if meta.get("openai/widgetDomain").is_some() {
        meta["openai/widgetDomain"] = Value::String(config.proxy_domain.clone());
    }

    // openai/widgetCSP → rewrite CSP arrays
    rewrite_csp_object(meta, "openai/widgetCSP", "resource_domains", config);
    rewrite_csp_object(meta, "openai/widgetCSP", "connect_domains", config);

    // ui.csp → rewrite CSP arrays (Claude format)
    if let Some(ui) = meta.get_mut("ui") {
        rewrite_csp_object(ui, "csp", "connectDomains", config);
        rewrite_csp_object(ui, "csp", "resourceDomains", config);
    }

    // Deep scan: ensure proxy URL is in ALL CSP domain arrays anywhere in the tree
    inject_proxy_into_all_csp(meta, config);
}

/// Recursively find any CSP domain arrays and ensure proxy URL is present.
fn inject_proxy_into_all_csp(value: &mut Value, config: &RewriteConfig) {
    match value {
        Value::Object(map) => {
            // Check known CSP array keys at this level
            for key in [
                "resource_domains",
                "connect_domains",
                "connectDomains",
                "resourceDomains",
            ] {
                if let Some(arr) = map.get_mut(key).and_then(|v| v.as_array_mut()) {
                    let has_proxy = arr.iter().any(|v| v.as_str() == Some(&config.proxy_url));
                    if !has_proxy {
                        arr.insert(0, Value::String(config.proxy_url.clone()));
                    }
                }
            }
            // Recurse into all values
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

/// Rewrite a CSP domain array inside a parent object.
/// - Extend mode: keep external domains from upstream, strip localhost/upstream, add extras + tunnel
/// - Override mode: ignore upstream entirely, use only configured domains + tunnel domain
fn rewrite_csp_object(parent: &mut Value, obj_key: &str, array_key: &str, config: &RewriteConfig) {
    let Some(obj) = parent.get_mut(obj_key) else {
        return;
    };
    let Some(arr) = obj.get_mut(array_key).and_then(|v| v.as_array_mut()) else {
        return;
    };

    // Always start with proxy (tunnel) domain
    let mut new_domains: Vec<String> = vec![config.proxy_url.clone()];

    match config.csp_mode {
        CspMode::Extend => {
            let upstream_domain = config
                .mcp_upstream
                .trim_start_matches("https://")
                .trim_start_matches("http://");

            for entry in arr.iter() {
                if let Some(domain) = entry.as_str() {
                    // Skip localhost/upstream — we replace them with proxy
                    if domain.contains("localhost")
                        || domain.contains("127.0.0.1")
                        || domain.contains(upstream_domain)
                    {
                        continue;
                    }
                    if !new_domains.contains(&domain.to_string()) {
                        new_domains.push(domain.to_string());
                    }
                }
            }
        }
        CspMode::Override => {
            // Ignore all upstream domains
        }
    }

    // Append extra CSP domains from config
    for extra in &config.extra_csp_domains {
        if !new_domains.contains(extra) {
            new_domains.push(extra.clone());
        }
    }

    *obj.get_mut(array_key).unwrap() =
        Value::Array(new_domains.into_iter().map(Value::String).collect());
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;
    use serde_json::json;

    fn test_config() -> RewriteConfig {
        RewriteConfig {
            proxy_url: "https://abc.tunnel.example.com".into(),
            proxy_domain: "abc.tunnel.example.com".into(),
            mcp_upstream: "http://localhost:9000".into(),
            extra_csp_domains: vec![],
            csp_mode: CspMode::Extend,
        }
    }

    // ── Standalone proxy mode: resources/read must NOT touch HTML text ──

    #[test]
    fn rewrite_response__resources_read_preserves_html() {
        let config = test_config();
        let mut body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": {
                "contents": [{
                    "uri": "ui://widget/question",
                    "mimeType": "text/html",
                    "text": "<html><script src=\"/assets/main.js\"></script></html>"
                }]
            }
        });
        let original_html = body["result"]["contents"][0]["text"]
            .as_str()
            .unwrap()
            .to_string();

        rewrite_response("resources/read", &mut body, &config);

        // HTML text must be untouched — rewrite only applies to meta
        let html = body["result"]["contents"][0]["text"].as_str().unwrap();
        assert_eq!(html, original_html);
    }

    #[test]
    fn rewrite_response__resources_read_rewrites_meta_not_text() {
        let config = test_config();
        let mut body = json!({
            "jsonrpc": "2.0",
            "id": 1,
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
        // Text untouched
        assert_eq!(
            content["text"].as_str().unwrap(),
            "<html><body>Hello</body></html>"
        );
        // Meta rewritten
        assert_eq!(
            content["meta"]["openai/widgetDomain"].as_str().unwrap(),
            "abc.tunnel.example.com"
        );
        let resource_domains: Vec<&str> = content["meta"]["openai/widgetCSP"]["resource_domains"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(resource_domains.contains(&"https://abc.tunnel.example.com"));
        assert!(!resource_domains.iter().any(|d| d.contains("localhost")));
    }

    // ── tools/list: rewrite widget meta on tools ──

    #[test]
    fn rewrite_response__tools_list_rewrites_widget_domain() {
        let config = test_config();
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
        // External domains preserved, localhost stripped
        let connect: Vec<&str> = meta["openai/widgetCSP"]["connect_domains"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(connect.contains(&"https://abc.tunnel.example.com"));
        assert!(connect.contains(&"https://api.external.com"));
        assert!(!connect.iter().any(|d| d.contains("localhost")));
    }

    // ── tools/call: rewrite meta ──

    #[test]
    fn rewrite_response__tools_call_rewrites_meta() {
        let config = test_config();
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
        // Tool result text content is NOT touched
        assert_eq!(
            body["result"]["content"][0]["text"].as_str().unwrap(),
            "some result"
        );
    }

    // ── resources/list: rewrite meta on each resource ──

    #[test]
    fn rewrite_response__resources_list_rewrites_meta() {
        let config = test_config();
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

    // ── resources/templates/list: rewrite meta on each template ──

    #[test]
    fn rewrite_response__resources_templates_list_rewrites_meta() {
        let config = test_config();
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
        let resource: Vec<&str> = meta["openai/widgetCSP"]["resource_domains"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(resource.contains(&"https://abc.tunnel.example.com"));
        assert!(!resource.iter().any(|d| d.contains("localhost")));
    }

    // ── CSP rewriting details ──

    #[test]
    fn rewrite_response__csp_strips_localhost() {
        let config = test_config();
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

        let domains: Vec<&str> =
            body["result"]["tools"][0]["meta"]["openai/widgetCSP"]["resource_domains"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap())
                .collect();
        assert_eq!(
            domains,
            vec!["https://abc.tunnel.example.com", "https://cdn.external.com"]
        );
    }

    #[test]
    fn rewrite_response__csp_extra_domains_appended() {
        let mut config = test_config();
        config.extra_csp_domains = vec!["https://extra.example.com".into()];

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

        let domains: Vec<&str> =
            body["result"]["tools"][0]["meta"]["openai/widgetCSP"]["connect_domains"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap())
                .collect();
        assert!(domains.contains(&"https://extra.example.com"));
    }

    #[test]
    fn rewrite_response__csp_no_duplicate_proxy() {
        let config = test_config();
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

        let domains: Vec<&str> =
            body["result"]["tools"][0]["meta"]["openai/widgetCSP"]["resource_domains"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap())
                .collect();
        let proxy_count = domains
            .iter()
            .filter(|d| **d == "https://abc.tunnel.example.com")
            .count();
        assert_eq!(proxy_count, 1);
    }

    // ── Claude format (ui.csp) ──

    #[test]
    fn rewrite_response__claude_csp_format() {
        let config = test_config();
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
        let connect: Vec<&str> = meta["connectDomains"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        let resource: Vec<&str> = meta["resourceDomains"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(connect.contains(&"https://abc.tunnel.example.com"));
        assert!(resource.contains(&"https://abc.tunnel.example.com"));
        assert!(!connect.iter().any(|d| d.contains("localhost")));
        assert!(!resource.iter().any(|d| d.contains("localhost")));
    }

    // ── Deep CSP injection ──

    #[test]
    fn rewrite_response__deep_csp_injection() {
        let config = test_config();
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

        let domains: Vec<&str> =
            body["result"]["content"][0]["deeply"]["nested"]["connect_domains"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap())
                .collect();
        assert!(domains.contains(&"https://abc.tunnel.example.com"));
    }

    // ── Unknown methods are passthrough ──

    #[test]
    fn rewrite_response__unknown_method_passthrough() {
        let config = test_config();
        let mut body = json!({
            "result": {
                "data": "unchanged",
                "meta": {
                    "openai/widgetDomain": "should-stay.com"
                }
            }
        });
        rewrite_response("notifications/message", &mut body, &config);

        // meta.openai/widgetDomain should NOT be rewritten for unknown methods
        // (only deep CSP injection runs, which doesn't touch widgetDomain)
        assert_eq!(
            body["result"]["meta"]["openai/widgetDomain"]
                .as_str()
                .unwrap(),
            "should-stay.com"
        );
        assert_eq!(body["result"]["data"].as_str().unwrap(), "unchanged");
    }

    // ── Override mode ──

    #[test]
    fn rewrite_response__override_mode_ignores_upstream() {
        let mut config = test_config();
        config.csp_mode = CspMode::Override;
        config.extra_csp_domains = vec!["https://allowed.example.com".into()];

        let mut body = json!({
            "result": {
                "tools": [{
                    "name": "test",
                    "meta": {
                        "openai/widgetCSP": {
                            "resource_domains": [
                                "https://cdn.external.com",
                                "https://api.external.com",
                                "http://localhost:4444"
                            ],
                            "connect_domains": [
                                "https://api.external.com",
                                "http://localhost:9000"
                            ]
                        }
                    }
                }]
            }
        });

        rewrite_response("tools/list", &mut body, &config);

        let resource: Vec<&str> =
            body["result"]["tools"][0]["meta"]["openai/widgetCSP"]["resource_domains"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap())
                .collect();
        // Only proxy + configured extras — no upstream domains
        assert_eq!(
            resource,
            vec![
                "https://abc.tunnel.example.com",
                "https://allowed.example.com"
            ]
        );

        let connect: Vec<&str> =
            body["result"]["tools"][0]["meta"]["openai/widgetCSP"]["connect_domains"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap())
                .collect();
        assert_eq!(
            connect,
            vec![
                "https://abc.tunnel.example.com",
                "https://allowed.example.com"
            ]
        );
    }

    #[test]
    fn rewrite_response__override_mode_claude_format() {
        let mut config = test_config();
        config.csp_mode = CspMode::Override;

        let mut body = json!({
            "result": {
                "tools": [{
                    "name": "test",
                    "meta": {
                        "ui": {
                            "csp": {
                                "connectDomains": ["https://api.external.com"],
                                "resourceDomains": ["https://cdn.external.com"]
                            }
                        }
                    }
                }]
            }
        });

        rewrite_response("tools/list", &mut body, &config);

        let connect: Vec<&str> = body["result"]["tools"][0]["meta"]["ui"]["csp"]["connectDomains"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        // Override: only proxy domain, no upstream
        assert_eq!(connect, vec!["https://abc.tunnel.example.com"]);
    }
}
