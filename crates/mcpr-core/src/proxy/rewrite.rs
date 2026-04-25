//! # Response rewriting for widget CSP
//!
//! Mutates JSON-RPC response bodies so widgets see the right CSP regardless of
//! which host renders them.
//!
//! Three things happen on every widget meta rewrite:
//!
//! 1. **Aggregate.** Upstream CSP domains are collected from both the OpenAI
//!    (`openai/widgetCSP`) and spec (`ui.csp`) shapes, per directive.
//! 2. **Merge.** [`super::csp::effective_domains`] applies the per-directive
//!    mode, widget-scoped overrides, and proxy URL to produce one domain list
//!    per directive.
//! 3. **Emit both shapes.** The merge result is written to *both*
//!    `openai/widgetCSP` (snake_case) and `ui.csp` (camelCase). ChatGPT reads
//!    the former, Claude and VS Code read the latter; unknown keys are
//!    ignored, so emitting both means the same declared config works on every
//!    host.
//!
//! A deep scan walks the entire response afterwards and prepends the proxy URL
//! to any CSP domain array it finds, catching servers that embed CSP in
//! non-standard locations.
//!
//! Response body `text` and `blob` fields are never touched — widget HTML is
//! served verbatim.

use serde_json::Value;

use super::csp::{CspConfig, Directive, effective_domains, is_public_proxy_origin};

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

impl RewriteConfig {
    /// Wrap this config in the lock-free `ArcSwap` shape expected by
    /// [`crate::proxy::ProxyState::rewrite_config`]. Saves every caller
    /// from importing `arc_swap` directly.
    pub fn into_swap(self) -> std::sync::Arc<arc_swap::ArcSwap<RewriteConfig>> {
        std::sync::Arc::new(arc_swap::ArcSwap::from_pointee(self))
    }
}

/// Rewrite a JSON-RPC response in place for the given method.
///
/// Returns `true` iff the body was actually mutated. Callers use this to
/// decide whether the response needs to be re-serialized — see
/// `pipeline::handlers::buffered` for the buffered-path dispatcher.
#[must_use]
pub fn rewrite_response(method: &str, body: &mut Value, config: &RewriteConfig) -> bool {
    let mut mutated = false;
    match method {
        "tools/list" => {
            if let Some(tools) = body
                .get_mut("result")
                .and_then(|r| r.get_mut("tools"))
                .and_then(|t| t.as_array_mut())
            {
                for tool in tools {
                    if let Some(meta) = tool.get_mut("_meta") {
                        rewrite_widget_meta(meta, None, config);
                        mutated = true;
                    }
                }
            }
        }
        "tools/call" => {
            if let Some(meta) = body.get_mut("result").and_then(|r| r.get_mut("_meta")) {
                rewrite_widget_meta(meta, None, config);
                mutated = true;
            }
        }
        "resources/list" => {
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
                    // A URI alone is enough to treat this as a widget resource
                    // per the MCP Apps spec, so synthesize `_meta` when the
                    // upstream omits it — otherwise declared CSP silently
                    // wouldn't apply to under-declaring servers.
                    let has_existing_meta = resource.get("_meta").is_some();
                    if (uri.is_some() || has_existing_meta)
                        && let Some(meta) = ensure_meta(resource)
                    {
                        rewrite_widget_meta(meta, uri.as_deref(), config);
                        mutated = true;
                    }
                }
            }
        }
        "resources/templates/list" => {
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
                    let has_existing_meta = template.get("_meta").is_some();
                    if (uri.is_some() || has_existing_meta)
                        && let Some(meta) = ensure_meta(template)
                    {
                        rewrite_widget_meta(meta, uri.as_deref(), config);
                        mutated = true;
                    }
                }
            }
        }
        "resources/read" => {
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
                    let has_existing_meta = content.get("_meta").is_some();
                    if (uri.is_some() || has_existing_meta)
                        && let Some(meta) = ensure_meta(content)
                    {
                        rewrite_widget_meta(meta, uri.as_deref(), config);
                        mutated = true;
                    }
                }
            }
        }
        _ => {}
    }

    // Safety net: any CSP-shaped domain array elsewhere in the tree still gets
    // the proxy URL. The merge rules above do not run here — this pass only
    // guarantees the proxy URL is present.
    mutated |= inject_proxy_into_all_csp(body, config);
    mutated
}

/// Rewrite a widget metadata object.
///
/// Goal: an operator declares their CSP once in `mcpr.toml`, and the widget
/// works on every host. Hosts disagree about where they read CSP from —
/// ChatGPT reads `openai/widgetCSP` (snake_case), Claude and VS Code read
/// `ui.csp` (camelCase). Instead of detecting the host, the rewrite does the
/// same merge once and emits the result to *both* shapes. Extra keys are
/// ignored by hosts that don't understand them.
///
/// Upstream domains that feed into the merge are aggregated from both shapes
/// so a server declaring only one shape still informs the merge for the other.
///
/// `explicit_uri` is the resource URI the caller already resolved (for example,
/// a `resources/read` caller knows the URI from the containing object). When
/// missing, the URI is inferred from `meta.ui.resourceUri` or the legacy
/// `openai/outputTemplate` field. The URI picks which `[[csp.widget]]`
/// overrides apply.
///
/// The rewrite is skipped entirely for meta objects that show no sign of being
/// a widget — a tool call result's `_meta`, for example — so non-widget metas
/// are not polluted with CSP fields they don't need.
fn rewrite_widget_meta(meta: &mut Value, explicit_uri: Option<&str>, config: &RewriteConfig) {
    // Hosts disagree about where they read the widget domain: ChatGPT reads
    // `openai/widgetDomain`, the MCP-UI / Apps spec reads `_meta.ui.domain`.
    // Same playbook as CSP — when the upstream declared the field in *either*
    // shape, emit our public domain into *both* so the widget is portable
    // across hosts.
    //
    // Two guards:
    //   * `proxy_domain` empty (local-only dev) — leave upstream alone rather
    //     than clobbering with "".
    //   * Neither shape declared upstream — never synthesize the key on a meta
    //     that didn't ask for it.
    if !config.proxy_domain.is_empty()
        && (meta.get("openai/widgetDomain").is_some() || meta.pointer("/ui/domain").is_some())
    {
        write_widget_domain(meta, &config.proxy_domain);
    }

    if !is_widget_meta(meta, explicit_uri) {
        // Inside rewrite_widget_meta: the caller's match arm has already
        // flagged mutation, so the return value is uninteresting here.
        let _ = inject_proxy_into_all_csp(meta, config);
        return;
    }

    let inferred = explicit_uri
        .map(String::from)
        .or_else(|| extract_resource_uri(meta));
    let uri = inferred.as_deref();
    let upstream_host = strip_scheme(&config.mcp_upstream);

    // Merge once per directive, using the union of upstream declarations from
    // both shapes so a server that only declared `ui.csp` still informs the
    // merge for the `openai/widgetCSP` output and vice versa.
    let connect = merged_domains(meta, Directive::Connect, uri, &upstream_host, config);
    let resource = merged_domains(meta, Directive::Resource, uri, &upstream_host, config);
    let frame = merged_domains(meta, Directive::Frame, uri, &upstream_host, config);

    write_openai_csp(meta, &connect, &resource, &frame);
    write_spec_csp(meta, &connect, &resource, &frame);

    // Same reasoning as above — caller already flags mutation.
    let _ = inject_proxy_into_all_csp(meta, config);
}

/// Return `true` when the meta object belongs to a widget, either because it
/// already holds widget-shaped fields or because the caller resolved an
/// explicit resource URI for it.
fn is_widget_meta(meta: &Value, explicit_uri: Option<&str>) -> bool {
    if explicit_uri.is_some() {
        return true;
    }
    meta.get("openai/widgetCSP").is_some()
        || meta.get("openai/widgetDomain").is_some()
        || meta.get("openai/outputTemplate").is_some()
        || meta.pointer("/ui/csp").is_some()
        || meta.pointer("/ui/resourceUri").is_some()
        || meta.pointer("/ui/domain").is_some()
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

/// Compute the effective domain list for one directive, seeded from whatever
/// the upstream server declared in either CSP shape.
fn merged_domains(
    meta: &Value,
    directive: Directive,
    resource_uri: Option<&str>,
    upstream_host: &str,
    config: &RewriteConfig,
) -> Vec<String> {
    let upstream = collect_upstream(meta, directive);
    effective_domains(
        &config.csp,
        directive,
        resource_uri,
        &upstream,
        upstream_host,
        &config.proxy_url,
    )
}

/// Gather every string domain the upstream declared for `directive`, looking
/// at both `openai/widgetCSP` and `ui.csp`. Duplicates are removed in order.
fn collect_upstream(meta: &Value, directive: Directive) -> Vec<String> {
    let (openai_key, spec_key) = match directive {
        Directive::Connect => ("connect_domains", "connectDomains"),
        Directive::Resource => ("resource_domains", "resourceDomains"),
        Directive::Frame => ("frame_domains", "frameDomains"),
    };

    let mut out: Vec<String> = Vec::new();
    let mut append = |arr: &Vec<Value>| {
        for v in arr {
            if let Some(s) = v.as_str() {
                let s = s.to_string();
                if !out.contains(&s) {
                    out.push(s);
                }
            }
        }
    };

    if let Some(arr) = meta
        .get("openai/widgetCSP")
        .and_then(|c| c.get(openai_key))
        .and_then(|v| v.as_array())
    {
        append(arr);
    }
    if let Some(arr) = meta
        .pointer("/ui/csp")
        .and_then(|c| c.get(spec_key))
        .and_then(|v| v.as_array())
    {
        append(arr);
    }
    out
}

/// Write the OpenAI-shaped CSP block, creating the parent object when needed.
/// Write the proxy's public domain into both widget-domain shapes
/// (`openai/widgetDomain` and `ui.domain`), creating `ui` when needed.
fn write_widget_domain(meta: &mut Value, domain: &str) {
    let Some(obj) = meta.as_object_mut() else {
        return;
    };
    obj.insert(
        "openai/widgetDomain".to_string(),
        Value::String(domain.to_string()),
    );
    let ui = obj
        .entry("ui".to_string())
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    if !ui.is_object() {
        *ui = Value::Object(serde_json::Map::new());
    }
    ui.as_object_mut()
        .unwrap()
        .insert("domain".to_string(), Value::String(domain.to_string()));
}

fn write_openai_csp(meta: &mut Value, connect: &[String], resource: &[String], frame: &[String]) {
    let Some(obj) = meta.as_object_mut() else {
        return;
    };
    obj.insert(
        "openai/widgetCSP".to_string(),
        serde_json::json!({
            "connect_domains": connect,
            "resource_domains": resource,
            "frame_domains": frame,
        }),
    );
}

/// Write the spec-shaped CSP block under `ui.csp`, creating `ui` when needed.
fn write_spec_csp(meta: &mut Value, connect: &[String], resource: &[String], frame: &[String]) {
    let Some(obj) = meta.as_object_mut() else {
        return;
    };
    let ui = obj
        .entry("ui".to_string())
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    if !ui.is_object() {
        *ui = Value::Object(serde_json::Map::new());
    }
    let ui_obj = ui.as_object_mut().unwrap();
    ui_obj.insert(
        "csp".to_string(),
        serde_json::json!({
            "connectDomains": connect,
            "resourceDomains": resource,
            "frameDomains": frame,
        }),
    );
}

/// Recursively ensure the proxy URL is present in every CSP-shaped domain array
/// that needs to reach the proxy (connect, resource). Frame arrays are skipped —
/// see `csp::effective_domains`.
///
/// Does not apply the merge rules — that would require URI context the deep scan
/// does not have. The only guarantee is "proxy URL is reachable from the widget."
/// Walk `value` and insert `config.proxy_url` at the front of every CSP
/// domain array that doesn't already contain it. Returns `true` iff any
/// insertion happened — lets callers skip re-serialization on no-op walks.
#[must_use]
fn inject_proxy_into_all_csp(value: &mut Value, config: &RewriteConfig) -> bool {
    // Skip entirely when there is no public origin worth injecting. A
    // localhost proxy URL in a submitted widget's CSP is useless to the
    // host (ChatGPT/Claude can't reach it) and just clutters the emitted
    // config — better to leave the arrays alone.
    if !is_public_proxy_origin(&config.proxy_url) {
        return false;
    }
    let mut mutated = false;
    match value {
        Value::Object(map) => {
            for key in [
                "connect_domains",
                "resource_domains",
                "connectDomains",
                "resourceDomains",
            ] {
                if let Some(arr) = map.get_mut(key).and_then(|v| v.as_array_mut()) {
                    let has_proxy = arr.iter().any(|v| v.as_str() == Some(&config.proxy_url));
                    if !has_proxy {
                        arr.insert(0, Value::String(config.proxy_url.clone()));
                        mutated = true;
                    }
                }
            }
            for (_, v) in map.iter_mut() {
                mutated |= inject_proxy_into_all_csp(v, config);
            }
        }
        Value::Array(arr) => {
            for item in arr {
                mutated |= inject_proxy_into_all_csp(item, config);
            }
        }
        _ => {}
    }
    mutated
}

fn strip_scheme(url: &str) -> String {
    url.trim_start_matches("https://")
        .trim_start_matches("http://")
        .split('/')
        .next()
        .unwrap_or("")
        .to_string()
}

/// Get `container._meta`, inserting an empty object if absent. `None` only when
/// the container isn't a JSON object — MCP resource/content entries always are,
/// so callers can treat `None` as a malformed upstream and skip.
fn ensure_meta(container: &mut Value) -> Option<&mut Value> {
    let obj = container.as_object_mut()?;
    Some(
        obj.entry("_meta".to_string())
            .or_insert_with(|| Value::Object(serde_json::Map::new())),
    )
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

        let _ = rewrite_response("resources/read", &mut body, &config);

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
                    "_meta": {
                        "openai/widgetDomain": "localhost:9000",
                        "openai/widgetCSP": {
                            "resource_domains": ["http://localhost:9000"],
                            "connect_domains": ["http://localhost:9000"]
                        }
                    }
                }]
            }
        });

        let _ = rewrite_response("resources/read", &mut body, &config);

        let content = &body["result"]["contents"][0];
        assert_eq!(
            content["text"].as_str().unwrap(),
            "<html><body>Hello</body></html>"
        );
        assert_eq!(
            content["_meta"]["openai/widgetDomain"].as_str().unwrap(),
            "abc.tunnel.example.com"
        );
        let resources = as_strs(&content["_meta"]["openai/widgetCSP"]["resource_domains"]);
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
                    "_meta": {
                        "openai/widgetDomain": "old.domain.com",
                        "openai/widgetCSP": {
                            "resource_domains": ["http://localhost:4444"],
                            "connect_domains": ["http://localhost:9000", "https://api.external.com"]
                        }
                    }
                }]
            }
        });

        let _ = rewrite_response("tools/list", &mut body, &config);

        let meta = &body["result"]["tools"][0]["_meta"];
        assert_eq!(
            meta["openai/widgetDomain"].as_str().unwrap(),
            "abc.tunnel.example.com"
        );
        let connect = as_strs(&meta["openai/widgetCSP"]["connect_domains"]);
        assert!(connect.contains(&"https://abc.tunnel.example.com"));
        assert!(connect.contains(&"https://api.external.com"));
        assert!(!connect.iter().any(|d| d.contains("localhost")));
    }

    #[test]
    fn rewrite_widget_meta__upstream_openai_only_emits_both_domain_shapes() {
        let config = rewrite_config();
        let mut meta = json!({
            "openai/widgetDomain": "old.domain.com"
        });

        rewrite_widget_meta(&mut meta, Some("ui://widget/x"), &config);

        assert_eq!(
            meta["openai/widgetDomain"].as_str().unwrap(),
            "abc.tunnel.example.com"
        );
        assert_eq!(
            meta["ui"]["domain"].as_str().unwrap(),
            "abc.tunnel.example.com"
        );
    }

    #[test]
    fn rewrite_widget_meta__upstream_ui_only_emits_both_domain_shapes() {
        let config = rewrite_config();
        let mut meta = json!({
            "ui": { "domain": "old.domain.com" }
        });

        rewrite_widget_meta(&mut meta, Some("ui://widget/x"), &config);

        assert_eq!(
            meta["ui"]["domain"].as_str().unwrap(),
            "abc.tunnel.example.com"
        );
        assert_eq!(
            meta["openai/widgetDomain"].as_str().unwrap(),
            "abc.tunnel.example.com"
        );
    }

    #[test]
    fn rewrite_widget_meta__no_upstream_domain_does_not_synthesize() {
        let config = rewrite_config();
        let mut meta = json!({
            "openai/widgetCSP": { "connect_domains": [] }
        });

        rewrite_widget_meta(&mut meta, Some("ui://widget/x"), &config);

        assert!(meta.get("openai/widgetDomain").is_none());
        assert!(meta.pointer("/ui/domain").is_none());
    }

    // ── tools/call: rewrites result.meta ───────────────────────────────────

    #[test]
    fn rewrite_response__tools_call_rewrites_meta() {
        let config = rewrite_config();
        let mut body = json!({
            "result": {
                "content": [{"type": "text", "text": "some result"}],
                "_meta": {
                    "openai/widgetDomain": "old.domain.com",
                    "openai/widgetCSP": {
                        "resource_domains": ["http://localhost:4444"]
                    }
                }
            }
        });

        let _ = rewrite_response("tools/call", &mut body, &config);

        assert_eq!(
            body["result"]["_meta"]["openai/widgetDomain"]
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
                    "_meta": {
                        "openai/widgetDomain": "old.domain.com"
                    }
                }]
            }
        });

        let _ = rewrite_response("resources/list", &mut body, &config);

        assert_eq!(
            body["result"]["resources"][0]["_meta"]["openai/widgetDomain"]
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
                    "_meta": {
                        "openai/widgetDomain": "old.domain.com",
                        "openai/widgetCSP": {
                            "resource_domains": ["http://localhost:4444"],
                            "connect_domains": ["http://localhost:9000"]
                        }
                    }
                }]
            }
        });

        let _ = rewrite_response("resources/templates/list", &mut body, &config);

        let meta = &body["result"]["resourceTemplates"][0]["_meta"];
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
                    "_meta": {
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

        let _ = rewrite_response("tools/list", &mut body, &config);

        let domains =
            as_strs(&body["result"]["tools"][0]["_meta"]["openai/widgetCSP"]["resource_domains"]);
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
                    "_meta": {
                        "openai/widgetCSP": {
                            "connect_domains": ["http://localhost:9000"]
                        }
                    }
                }]
            }
        });

        let _ = rewrite_response("tools/list", &mut body, &config);

        let domains =
            as_strs(&body["result"]["tools"][0]["_meta"]["openai/widgetCSP"]["connect_domains"]);
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
                    "_meta": {
                        "openai/widgetCSP": {
                            "resource_domains": ["https://abc.tunnel.example.com", "https://cdn.example.com"]
                        }
                    }
                }]
            }
        });

        let _ = rewrite_response("tools/list", &mut body, &config);

        let domains =
            as_strs(&body["result"]["tools"][0]["_meta"]["openai/widgetCSP"]["resource_domains"]);
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
                    "_meta": {
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

        let _ = rewrite_response("tools/list", &mut body, &config);

        let meta = &body["result"]["tools"][0]["_meta"]["ui"]["csp"];
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

        let _ = rewrite_response("tools/call", &mut body, &config);

        let domains = as_strs(&body["result"]["content"][0]["deeply"]["nested"]["connect_domains"]);
        assert!(domains.contains(&"https://abc.tunnel.example.com"));
    }

    #[test]
    fn rewrite_response__deep_csp_injection_skips_frame_arrays() {
        // Regression guard for the frame-domains fix: the safety-net deep scan
        // must not prepend the proxy URL to frame arrays, or every mcpr-proxied
        // widget would look like an iframe-embedder to ChatGPT and trigger
        // extra security review.
        let config = rewrite_config();
        let mut body = json!({
            "result": {
                "content": [{
                    "type": "text",
                    "text": "result",
                    "deeply": {
                        "nested": {
                            "frame_domains": ["https://embed.partner.com"],
                            "frameDomains": ["https://embed.partner.com"]
                        }
                    }
                }]
            }
        });

        let _ = rewrite_response("tools/call", &mut body, &config);

        let nested = &body["result"]["content"][0]["deeply"]["nested"];
        let snake = as_strs(&nested["frame_domains"]);
        let camel = as_strs(&nested["frameDomains"]);
        assert_eq!(snake, vec!["https://embed.partner.com"]);
        assert_eq!(camel, vec!["https://embed.partner.com"]);
    }

    // ── Unknown methods: only deep scan runs ───────────────────────────────

    #[test]
    fn rewrite_response__unknown_method_passthrough() {
        let config = rewrite_config();
        let mut body = json!({
            "result": {
                "data": "unchanged",
                "_meta": { "openai/widgetDomain": "should-stay.com" }
            }
        });
        let _ = rewrite_response("notifications/message", &mut body, &config);

        assert_eq!(
            body["result"]["_meta"]["openai/widgetDomain"]
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
                    "_meta": {
                        "openai/widgetCSP": {
                            "resource_domains": ["https://cdn.external.com", "https://api.external.com"],
                            "connect_domains": ["https://api.external.com", "http://localhost:9000"]
                        }
                    }
                }]
            }
        });

        let _ = rewrite_response("tools/list", &mut body, &config);

        let resources =
            as_strs(&body["result"]["tools"][0]["_meta"]["openai/widgetCSP"]["resource_domains"]);
        assert_eq!(
            resources,
            vec![
                "https://abc.tunnel.example.com",
                "https://allowed.example.com"
            ]
        );
        let connect =
            as_strs(&body["result"]["tools"][0]["_meta"]["openai/widgetCSP"]["connect_domains"]);
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
                        "_meta": {
                            "openai/widgetCSP": { "connect_domains": [] }
                        }
                    },
                    {
                        "uri": "ui://widget/search",
                        "_meta": {
                            "openai/widgetCSP": { "connect_domains": [] }
                        }
                    }
                ]
            }
        });

        let _ = rewrite_response("resources/list", &mut body, &config);

        let payment_connect = as_strs(
            &body["result"]["resources"][0]["_meta"]["openai/widgetCSP"]["connect_domains"],
        );
        assert!(payment_connect.contains(&"https://api.stripe.com"));

        let search_connect = as_strs(
            &body["result"]["resources"][1]["_meta"]["openai/widgetCSP"]["connect_domains"],
        );
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
                    "_meta": {
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

        let _ = rewrite_response("resources/read", &mut body, &config);

        let connect =
            as_strs(&body["result"]["contents"][0]["_meta"]["openai/widgetCSP"]["connect_domains"]);
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
                    "_meta": {
                        "ui": { "resourceUri": "ui://widget/payment-form" },
                        "openai/widgetCSP": { "connect_domains": [] }
                    }
                }]
            }
        });

        let _ = rewrite_response("tools/list", &mut body, &config);

        let connect =
            as_strs(&body["result"]["tools"][0]["_meta"]["openai/widgetCSP"]["connect_domains"]);
        assert!(connect.contains(&"https://api.stripe.com"));
    }

    // ── Both shapes emitted from one declaration ───────────────────────────

    #[test]
    fn rewrite_response__spec_only_upstream_also_emits_openai_shape() {
        // Upstream only declared ui.csp (spec shape). The rewrite must also
        // synthesize openai/widgetCSP so ChatGPT — which reads the legacy
        // key — receives the same effective CSP.
        let config = rewrite_config();
        let mut body = json!({
            "result": {
                "contents": [{
                    "uri": "ui://widget/search",
                    "mimeType": "text/html",
                    "_meta": {
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

        let _ = rewrite_response("resources/read", &mut body, &config);

        let meta = &body["result"]["contents"][0]["_meta"];
        let oa_connect = as_strs(&meta["openai/widgetCSP"]["connect_domains"]);
        let spec_connect = as_strs(&meta["ui"]["csp"]["connectDomains"]);
        assert_eq!(oa_connect, spec_connect);
        assert!(oa_connect.contains(&"https://api.external.com"));
        assert!(oa_connect.contains(&"https://abc.tunnel.example.com"));
    }

    #[test]
    fn rewrite_response__openai_only_upstream_also_emits_spec_shape() {
        // Reverse of the above: upstream only declared openai/widgetCSP and
        // Claude/VS Code clients must still see ui.csp.
        let config = rewrite_config();
        let mut body = json!({
            "result": {
                "contents": [{
                    "uri": "ui://widget/search",
                    "mimeType": "text/html",
                    "_meta": {
                        "openai/widgetCSP": {
                            "connect_domains": ["https://api.external.com"],
                            "resource_domains": ["https://cdn.external.com"]
                        }
                    }
                }]
            }
        });

        let _ = rewrite_response("resources/read", &mut body, &config);

        let meta = &body["result"]["contents"][0]["_meta"];
        let oa_connect = as_strs(&meta["openai/widgetCSP"]["connect_domains"]);
        let spec_connect = as_strs(&meta["ui"]["csp"]["connectDomains"]);
        assert_eq!(oa_connect, spec_connect);
        assert!(spec_connect.contains(&"https://api.external.com"));
        assert!(spec_connect.contains(&"https://abc.tunnel.example.com"));
    }

    #[test]
    fn rewrite_response__declared_config_synthesizes_both_shapes_from_empty() {
        // The server declared neither CSP shape. The operator's mcpr.toml
        // declares connectDomains. Both shapes appear in the response with
        // the declared domain, keyed off the URI on the containing resource.
        let mut config = rewrite_config();
        config.csp.connect_domains = DirectivePolicy {
            domains: vec!["https://api.declared.com".into()],
            mode: Mode::Extend,
        };

        let mut body = json!({
            "result": {
                "resources": [{
                    "uri": "ui://widget/search",
                    "_meta": {
                        "openai/widgetDomain": "old.domain.com"
                    }
                }]
            }
        });

        let _ = rewrite_response("resources/list", &mut body, &config);

        let meta = &body["result"]["resources"][0]["_meta"];
        let oa = as_strs(&meta["openai/widgetCSP"]["connect_domains"]);
        let spec = as_strs(&meta["ui"]["csp"]["connectDomains"]);
        assert_eq!(oa, spec);
        assert!(oa.contains(&"https://api.declared.com"));
        assert!(oa.contains(&"https://abc.tunnel.example.com"));
    }

    #[test]
    fn rewrite_response__upstream_declarations_unioned_across_shapes() {
        // The server filled different domains into each shape. The merge must
        // see the union, not pick one.
        let config = rewrite_config();
        let mut body = json!({
            "result": {
                "contents": [{
                    "uri": "ui://widget/search",
                    "mimeType": "text/html",
                    "_meta": {
                        "openai/widgetCSP": {
                            "connect_domains": ["https://api.only-openai.com"]
                        },
                        "ui": {
                            "csp": {
                                "connectDomains": ["https://api.only-spec.com"]
                            }
                        }
                    }
                }]
            }
        });

        let _ = rewrite_response("resources/read", &mut body, &config);

        let meta = &body["result"]["contents"][0]["_meta"];
        let oa = as_strs(&meta["openai/widgetCSP"]["connect_domains"]);
        let spec = as_strs(&meta["ui"]["csp"]["connectDomains"]);
        assert_eq!(oa, spec);
        assert!(oa.contains(&"https://api.only-openai.com"));
        assert!(oa.contains(&"https://api.only-spec.com"));
    }

    #[test]
    fn rewrite_response__non_widget_meta_is_not_polluted() {
        // A tool call result with plain meta (no widget indicators) must not
        // gain synthesized CSP fields — those only belong on widget metas.
        let config = rewrite_config();
        let mut body = json!({
            "result": {
                "content": [{"type": "text", "text": "plain result"}],
                "_meta": { "requestId": "abc-123" }
            }
        });

        let _ = rewrite_response("tools/call", &mut body, &config);

        let meta = &body["result"]["_meta"];
        assert!(meta.get("openai/widgetCSP").is_none());
        assert!(meta.get("ui").is_none());
        assert_eq!(meta["requestId"].as_str().unwrap(), "abc-123");
    }

    #[test]
    fn rewrite_response__all_three_directives_synthesized() {
        // All three directives land in both shapes, not just connect.
        let mut config = rewrite_config();
        config.csp.connect_domains = DirectivePolicy {
            domains: vec!["https://api.example.com".into()],
            mode: Mode::Extend,
        };
        config.csp.resource_domains = DirectivePolicy {
            domains: vec!["https://cdn.example.com".into()],
            mode: Mode::Extend,
        };

        let mut body = json!({
            "result": {
                "resources": [{
                    "uri": "ui://widget/search",
                    "_meta": { "openai/widgetDomain": "x" }
                }]
            }
        });

        let _ = rewrite_response("resources/list", &mut body, &config);

        let meta = &body["result"]["resources"][0]["_meta"];
        let shape = "openai/widgetCSP";
        assert!(meta[shape]["connect_domains"].is_array());
        assert!(meta[shape]["resource_domains"].is_array());
        assert!(meta[shape]["frame_domains"].is_array());
        assert!(meta["ui"]["csp"]["connectDomains"].is_array());
        assert!(meta["ui"]["csp"]["resourceDomains"].is_array());
        assert!(meta["ui"]["csp"]["frameDomains"].is_array());
    }

    // ── Frame directive defaults strict ────────────────────────────────────

    #[test]
    fn rewrite_response__frame_domains_default_replace_drops_upstream() {
        // Default config treats frameDomains as replace, so upstream values are
        // dropped. Unlike connect/resource, the proxy URL is NOT prepended to
        // frame — the widget doesn't iframe the proxy back into itself, and
        // prepending it flags the widget as an iframe-embedder to hosts.
        let config = rewrite_config();
        let mut body = json!({
            "result": {
                "tools": [{
                    "name": "test",
                    "_meta": {
                        "ui": {
                            "csp": {
                                "frameDomains": ["https://embed.external.com"]
                            }
                        }
                    }
                }]
            }
        });

        let _ = rewrite_response("tools/list", &mut body, &config);

        let frames = as_strs(&body["result"]["tools"][0]["_meta"]["ui"]["csp"]["frameDomains"]);
        assert!(
            frames.is_empty(),
            "frame_domains should be empty, got {frames:?}"
        );
    }

    // ── End-to-end scenario ────────────────────────────────────────────────

    #[test]
    fn rewrite_response__end_to_end_mcp_schema() {
        // Scenario exercising the full pipeline:
        // - A realistic multi-tool tools/list response from an upstream MCP
        //   server that declares widgets with mixed metadata shapes.
        // - A mcpr.toml with declared global CSP across all three directives
        //   and a widget-scoped override for the payment widget.
        //
        // After rewriting, every tool's meta should carry both CSP shapes
        // populated from the same merge, with upstream self-references
        // dropped, declared config applied, and the payment widget's Stripe
        // override only appearing on the payment tool.
        let mut config = rewrite_config();
        config.csp.connect_domains = DirectivePolicy {
            domains: vec!["https://api.myshop.com".into()],
            mode: Mode::Extend,
        };
        config.csp.resource_domains = DirectivePolicy {
            domains: vec!["https://cdn.myshop.com".into()],
            mode: Mode::Extend,
        };
        config.csp.widgets.push(WidgetScoped {
            match_pattern: "ui://widget/payment*".into(),
            connect_domains: vec!["https://api.stripe.com".into()],
            connect_domains_mode: Mode::Extend,
            resource_domains: vec!["https://js.stripe.com".into()],
            resource_domains_mode: Mode::Extend,
            ..Default::default()
        });

        let mut body = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "result": {
                "tools": [
                    {
                        "name": "search_products",
                        "description": "Search the product catalog",
                        "inputSchema": { "type": "object" },
                        "_meta": {
                            "openai/widgetDomain": "old.shop.com",
                            "openai/outputTemplate": "ui://widget/search",
                            "openai/widgetCSP": {
                                "connect_domains": ["http://localhost:9000"],
                                "resource_domains": ["http://localhost:4444"]
                            }
                        }
                    },
                    {
                        "name": "take_payment",
                        "description": "Charge a card",
                        "inputSchema": { "type": "object" },
                        "_meta": {
                            "ui": {
                                "resourceUri": "ui://widget/payment-form",
                                "csp": {
                                    "connectDomains": ["https://api.myshop.com"]
                                }
                            }
                        }
                    },
                    {
                        "name": "get_order_status",
                        "description": "Look up an order",
                        "inputSchema": { "type": "object" }
                    }
                ]
            }
        });

        let _ = rewrite_response("tools/list", &mut body, &config);

        let tools = body["result"]["tools"].as_array().unwrap();

        // ── Tool 0: search — upstream declared only OpenAI shape ──────────
        let search_meta = &tools[0]["_meta"];
        assert_eq!(
            search_meta["openai/widgetDomain"].as_str().unwrap(),
            "abc.tunnel.example.com"
        );
        let search_oa_connect = as_strs(&search_meta["openai/widgetCSP"]["connect_domains"]);
        let search_spec_connect = as_strs(&search_meta["ui"]["csp"]["connectDomains"]);
        assert_eq!(search_oa_connect, search_spec_connect);
        // Proxy first, then declared global, upstream localhost dropped.
        assert_eq!(
            search_oa_connect,
            vec!["https://abc.tunnel.example.com", "https://api.myshop.com"]
        );
        // The payment widget override must NOT apply to the search tool.
        assert!(!search_oa_connect.contains(&"https://api.stripe.com"));
        // Frame directive defaults strict AND the proxy URL is not prepended
        // — so with no declared frame domains the array is fully empty.
        let search_oa_frame = as_strs(&search_meta["openai/widgetCSP"]["frame_domains"]);
        assert!(search_oa_frame.is_empty());

        // ── Tool 1: payment — upstream declared only spec shape ──────────
        let payment_meta = &tools[1]["_meta"];
        let payment_oa_connect = as_strs(&payment_meta["openai/widgetCSP"]["connect_domains"]);
        let payment_spec_connect = as_strs(&payment_meta["ui"]["csp"]["connectDomains"]);
        assert_eq!(payment_oa_connect, payment_spec_connect);
        // Proxy + global + widget override (Stripe) all present, in that order.
        assert_eq!(
            payment_oa_connect,
            vec![
                "https://abc.tunnel.example.com",
                "https://api.myshop.com",
                "https://api.stripe.com",
            ]
        );
        let payment_oa_resource = as_strs(&payment_meta["openai/widgetCSP"]["resource_domains"]);
        assert_eq!(
            payment_oa_resource,
            vec![
                "https://abc.tunnel.example.com",
                "https://cdn.myshop.com",
                "https://js.stripe.com",
            ]
        );

        // ── Tool 2: plain tool, no widget metadata ────────────────────────
        // Non-widget metas must not gain synthesized CSP fields.
        let plain = &tools[2];
        assert!(plain.get("_meta").is_none());
    }

    // ── Real MCP wire shape: `_meta` key, not `meta` ──────────────────────

    #[test]
    fn rewrite_response__tools_call_underscore_meta_is_rewritten() {
        // Regression: real MCP servers emit `_meta` (with underscore) per spec,
        // not `meta`. Earlier dispatch arms mistakenly read `meta`, silently
        // skipping every rewrite in production.
        let mut config = rewrite_config();
        config.csp.connect_domains = DirectivePolicy {
            domains: vec!["https://assets.usestudykit.com".into()],
            mode: Mode::Replace,
        };
        config.csp.resource_domains = DirectivePolicy {
            domains: vec!["https://assets.usestudykit.com".into()],
            mode: Mode::Replace,
        };

        let mut body = json!({
            "result": {
                "_meta": {
                    "openai/outputTemplate": "ui://widget/vocab_review.html",
                    "openai/widgetDomain": "assets.usestudykit.com/src",
                    "openai/widgetCSP": {
                        "connect_domains": [
                            "http://localhost:9002",
                            "https://api.dictionaryapi.dev"
                        ],
                        "resource_domains": [
                            "http://localhost:9002",
                            "https://api.dictionaryapi.dev"
                        ]
                    },
                    "ui": {
                        "csp": {
                            "connectDomains": ["https://api.dictionaryapi.dev"],
                            "resourceDomains": ["https://api.dictionaryapi.dev"]
                        },
                        "resourceUri": "ui://widget/vocab_review.html"
                    }
                },
                "content": [{"type": "text", "text": "payload"}],
                "structuredContent": {"data": {"items": []}}
            }
        });

        let _ = rewrite_response("tools/call", &mut body, &config);

        let meta = &body["result"]["_meta"];
        assert_eq!(
            meta["openai/widgetDomain"].as_str().unwrap(),
            "abc.tunnel.example.com"
        );
        let oa_connect = as_strs(&meta["openai/widgetCSP"]["connect_domains"]);
        let spec_connect = as_strs(&meta["ui"]["csp"]["connectDomains"]);
        assert_eq!(oa_connect, spec_connect);
        assert_eq!(
            oa_connect,
            vec![
                "https://abc.tunnel.example.com",
                "https://assets.usestudykit.com"
            ]
        );
        let oa_resource = as_strs(&meta["openai/widgetCSP"]["resource_domains"]);
        assert_eq!(
            oa_resource,
            vec![
                "https://abc.tunnel.example.com",
                "https://assets.usestudykit.com"
            ]
        );
        assert_eq!(
            body["result"]["content"][0]["text"].as_str().unwrap(),
            "payload"
        );
    }

    #[test]
    fn rewrite_response__resources_read_underscore_meta_is_rewritten() {
        let config = rewrite_config();
        let mut body = json!({
            "result": {
                "contents": [{
                    "uri": "ui://widget/question",
                    "mimeType": "text/html",
                    "text": "<html/>",
                    "_meta": {
                        "openai/widgetDomain": "old.domain.com"
                    }
                }]
            }
        });

        let _ = rewrite_response("resources/read", &mut body, &config);

        assert_eq!(
            body["result"]["contents"][0]["_meta"]["openai/widgetDomain"]
                .as_str()
                .unwrap(),
            "abc.tunnel.example.com"
        );
    }

    #[test]
    fn rewrite_response__legacy_meta_key_is_ignored() {
        // Defensive: if an upstream sends the wrong key (`meta` without
        // underscore), we must not rewrite it — MCP spec uses `_meta` only.
        let config = rewrite_config();
        let mut body = json!({
            "result": {
                "_meta": {"openai/widgetDomain": "real.domain.com"},
                "meta":  {"openai/widgetDomain": "should-stay.com"}
            }
        });

        let _ = rewrite_response("tools/call", &mut body, &config);

        assert_eq!(
            body["result"]["_meta"]["openai/widgetDomain"]
                .as_str()
                .unwrap(),
            "abc.tunnel.example.com"
        );
        assert_eq!(
            body["result"]["meta"]["openai/widgetDomain"]
                .as_str()
                .unwrap(),
            "should-stay.com"
        );
    }

    // ── _meta synthesis when upstream under-declares ──────────────────────

    #[test]
    fn rewrite_response__resources_list_synthesizes_meta_when_upstream_omits() {
        // Widget resources with no `_meta` at all must still receive the
        // declared CSP — under-declaring servers are common in practice.
        let mut config = rewrite_config();
        config.csp.connect_domains = DirectivePolicy {
            domains: vec!["https://api.declared.com".into()],
            mode: Mode::Replace,
        };
        config.csp.resource_domains = DirectivePolicy {
            domains: vec!["https://cdn.declared.com".into()],
            mode: Mode::Extend,
        };

        let mut body = json!({
            "result": {
                "resources": [{
                    "uri": "ui://widget/search",
                    "name": "Search Widget"
                }]
            }
        });

        let mutated = rewrite_response("resources/list", &mut body, &config);
        assert!(mutated);

        let meta = &body["result"]["resources"][0]["_meta"];
        let oa_connect = as_strs(&meta["openai/widgetCSP"]["connect_domains"]);
        let spec_connect = as_strs(&meta["ui"]["csp"]["connectDomains"]);
        assert_eq!(oa_connect, spec_connect);
        assert_eq!(
            oa_connect,
            vec!["https://abc.tunnel.example.com", "https://api.declared.com"]
        );
        let oa_resource = as_strs(&meta["openai/widgetCSP"]["resource_domains"]);
        assert!(oa_resource.contains(&"https://abc.tunnel.example.com"));
        assert!(oa_resource.contains(&"https://cdn.declared.com"));
    }

    #[test]
    fn rewrite_response__resources_read_synthesizes_meta_when_upstream_omits() {
        let mut config = rewrite_config();
        config.csp.connect_domains = DirectivePolicy {
            domains: vec!["https://api.declared.com".into()],
            mode: Mode::Replace,
        };

        let mut body = json!({
            "result": {
                "contents": [{
                    "uri": "ui://widget/question",
                    "mimeType": "text/html",
                    "text": "<html><body>Hello</body></html>"
                }]
            }
        });

        let mutated = rewrite_response("resources/read", &mut body, &config);
        assert!(mutated);

        assert_eq!(
            body["result"]["contents"][0]["text"].as_str().unwrap(),
            "<html><body>Hello</body></html>"
        );
        let meta = &body["result"]["contents"][0]["_meta"];
        let oa = as_strs(&meta["openai/widgetCSP"]["connect_domains"]);
        let spec = as_strs(&meta["ui"]["csp"]["connectDomains"]);
        assert_eq!(oa, spec);
        assert_eq!(
            oa,
            vec!["https://abc.tunnel.example.com", "https://api.declared.com"]
        );
    }

    #[test]
    fn rewrite_response__resources_list_injects_into_empty_meta() {
        let mut config = rewrite_config();
        config.csp.connect_domains = DirectivePolicy {
            domains: vec!["https://api.declared.com".into()],
            mode: Mode::Extend,
        };

        let mut body = json!({
            "result": {
                "resources": [{
                    "uri": "ui://widget/search",
                    "_meta": {}
                }]
            }
        });

        let _ = rewrite_response("resources/list", &mut body, &config);

        let meta = &body["result"]["resources"][0]["_meta"];
        let oa = as_strs(&meta["openai/widgetCSP"]["connect_domains"]);
        assert!(oa.contains(&"https://api.declared.com"));
        assert!(oa.contains(&"https://abc.tunnel.example.com"));
    }

    #[test]
    fn rewrite_response__resources_templates_list_synthesizes_meta() {
        let mut config = rewrite_config();
        config.csp.resource_domains = DirectivePolicy {
            domains: vec!["https://cdn.declared.com".into()],
            mode: Mode::Extend,
        };

        let mut body = json!({
            "result": {
                "resourceTemplates": [{
                    "uriTemplate": "ui://widget/{name}.html",
                    "name": "Widget Template"
                }]
            }
        });

        let _ = rewrite_response("resources/templates/list", &mut body, &config);

        let meta = &body["result"]["resourceTemplates"][0]["_meta"];
        let oa = as_strs(&meta["openai/widgetCSP"]["resource_domains"]);
        assert!(oa.contains(&"https://cdn.declared.com"));
        assert!(oa.contains(&"https://abc.tunnel.example.com"));
    }

    #[test]
    fn rewrite_response__tools_call_no_meta_is_not_synthesized() {
        // Spec-aligned asymmetry: tools/call without widget indicators must
        // NOT grow synthesized CSP fields — CSP belongs on widget resources.
        let mut config = rewrite_config();
        config.csp.connect_domains = DirectivePolicy {
            domains: vec!["https://api.declared.com".into()],
            mode: Mode::Replace,
        };

        let mut body = json!({
            "result": {
                "content": [{"type": "text", "text": "London 14C"}],
                "structuredContent": {"city": "London", "temp": 14}
            }
        });

        let _ = rewrite_response("tools/call", &mut body, &config);

        assert!(body["result"].get("_meta").is_none());
        assert_eq!(
            body["result"]["content"][0]["text"].as_str().unwrap(),
            "London 14C"
        );
    }

    #[test]
    fn rewrite_response__resources_list_skips_when_no_uri_and_no_meta() {
        let config = rewrite_config();
        let mut body = json!({
            "result": {
                "resources": [{
                    "name": "malformed"
                }]
            }
        });

        let _ = rewrite_response("resources/list", &mut body, &config);

        assert!(body["result"]["resources"][0].get("_meta").is_none());
    }

    // ── local-only mode: no public origin, don't pollute widget CSP ────────

    fn local_only_config() -> RewriteConfig {
        // Mirrors what main.rs builds when tunnel is off and
        // `csp.domain` is unset: proxy_url stays as the local
        // bind address for internal wiring, proxy_domain is empty to flag
        // "no public origin".
        RewriteConfig {
            proxy_url: "http://localhost:9002".into(),
            proxy_domain: String::new(),
            mcp_upstream: "http://localhost:9000".into(),
            csp: CspConfig::default(),
        }
    }

    #[test]
    fn rewrite_response__local_only_leaves_widget_domain_untouched() {
        let config = local_only_config();
        let mut body = json!({
            "result": {
                "contents": [{
                    "uri": "ui://widget/card",
                    "_meta": {
                        "openai/widgetDomain": "dev.example.com"
                    }
                }]
            }
        });

        let _ = rewrite_response("resources/read", &mut body, &config);

        assert_eq!(
            body["result"]["contents"][0]["_meta"]["openai/widgetDomain"]
                .as_str()
                .unwrap(),
            "dev.example.com",
        );
    }

    #[test]
    fn rewrite_response__local_only_leaves_ui_domain_untouched() {
        let config = local_only_config();
        let mut body = json!({
            "result": {
                "contents": [{
                    "uri": "ui://widget/card",
                    "_meta": {
                        "ui": { "domain": "dev.example.com" }
                    }
                }]
            }
        });

        let _ = rewrite_response("resources/read", &mut body, &config);

        assert_eq!(
            body["result"]["contents"][0]["_meta"]["ui"]["domain"]
                .as_str()
                .unwrap(),
            "dev.example.com",
        );
        assert!(
            body["result"]["contents"][0]["_meta"]
                .get("openai/widgetDomain")
                .is_none(),
            "must not synthesize the openai shape in local-only mode"
        );
    }

    #[test]
    fn rewrite_response__local_only_skips_csp_injection() {
        let config = local_only_config();
        let mut body = json!({
            "result": {
                "contents": [{
                    "uri": "ui://widget/card",
                    "_meta": {
                        "openai/widgetCSP": {
                            "connect_domains": ["https://api.example.com"],
                            "resource_domains": ["https://cdn.example.com"],
                            "frame_domains": []
                        }
                    }
                }]
            }
        });

        let _ = rewrite_response("resources/read", &mut body, &config);

        let csp = &body["result"]["contents"][0]["_meta"]["openai/widgetCSP"];
        assert_eq!(
            as_strs(&csp["connect_domains"]),
            vec!["https://api.example.com"],
            "localhost proxy_url must not be injected",
        );
        assert_eq!(
            as_strs(&csp["resource_domains"]),
            vec!["https://cdn.example.com"],
        );
    }

    #[test]
    fn rewrite_response__public_domain_is_injected() {
        // Sanity: with a public proxy origin, injection still happens.
        let config = rewrite_config();
        let mut body = json!({
            "result": {
                "contents": [{
                    "uri": "ui://widget/card",
                    "_meta": {
                        "openai/widgetCSP": {
                            "connect_domains": ["https://api.example.com"],
                            "resource_domains": [],
                            "frame_domains": []
                        }
                    }
                }]
            }
        });

        let _ = rewrite_response("resources/read", &mut body, &config);

        let connect =
            as_strs(&body["result"]["contents"][0]["_meta"]["openai/widgetCSP"]["connect_domains"]);
        assert!(connect.contains(&"https://abc.tunnel.example.com"));
    }
}
