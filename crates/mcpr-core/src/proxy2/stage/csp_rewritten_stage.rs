//! Response stage that rewrites widget CSP directives in MCP results.
//!
//! Mirrors [`crate::csp::effective_domains`] over the response shape:
//! `tools/list` (`result.tools[]._meta`), `tools/call` (`result._meta`),
//! `resources/list` (`result.resources[]._meta`), `resources/templates/list`,
//! and `resources/read` (`result.contents[]._meta`) all flow through the
//! same widget-meta rewrite. Method context isn't plumbed into the response
//! stage, so the dispatch is shape-driven instead — the result keys are
//! mutually exclusive across these methods.
//!
//! Both CSP shapes are emitted from a single declared config: ChatGPT reads
//! `openai/widgetCSP` (snake_case) and Claude/VS Code read `ui.csp`
//! (camelCase). Unknown keys are ignored, so emitting both means one
//! declaration works on every host.

use std::sync::Arc;

use arc_swap::ArcSwap;
use async_trait::async_trait;
use serde_json::{Map, Value};

use crate::{
    protocol::{Response, mcp::JsonRpcResult},
    proxy2::csp::{CspConfig, Directive, effective_domains, is_public_proxy_origin},
    proxy2::{proxy_config::ProxyConfig, stage::types::ResponseStage, state::ProxyState},
};

/// Inputs to the CSP rewrite. Held inside an `ArcSwap` on the stage so
/// `mcpr.toml` reloads can swap the inner `Arc` without restarting the proxy.
#[derive(Clone, Debug)]
pub struct CspRewriteConfig {
    /// Proxy URL (scheme + host, no trailing slash) to insert into every CSP
    /// array so widgets can reach the proxy.
    pub proxy_url: String,
    /// Bare proxy host (no scheme) written into `openai/widgetDomain`.
    /// `_meta.ui.domain` is intentionally never written — Claude derives that
    /// field from the proxy URL itself and rejects mismatching values.
    pub proxy_domain: String,
    /// Upstream MCP URL, used to recognise and strip upstream self-references
    /// from the CSP arrays the server returns.
    pub mcp_upstream: String,
    /// Declarative CSP config — global policies plus widget-scoped overrides.
    pub csp: CspConfig,
}

impl CspRewriteConfig {
    /// Derive runtime inputs from a resolved [`ProxyConfig`].
    ///
    /// `csp.domain` carries the operator-declared public host. When it is set
    /// we build `https://{domain}` and inject it into widget CSP. When it is
    /// absent we fall back to a loopback URL paired with an empty
    /// `proxy_domain` — the same "no public origin" signal the rewrite checks
    /// via [`crate::csp::is_public_proxy_origin`]. The exact port is
    /// irrelevant in that case because injection is suppressed for loopback,
    /// so we don't need the bound port to be known yet.
    pub fn from_proxy_config(cfg: &ProxyConfig) -> Self {
        let proxy_domain = cfg.csp.domain.clone().unwrap_or_default();
        let proxy_url = if proxy_domain.is_empty() {
            "http://localhost".to_string()
        } else {
            format!("https://{proxy_domain}")
        };
        Self {
            proxy_url,
            proxy_domain,
            mcp_upstream: cfg.mcp.clone(),
            csp: cfg.csp.clone(),
        }
    }
}

pub struct CspRewritter {
    config: Arc<ArcSwap<CspRewriteConfig>>,
}

impl CspRewritter {
    pub fn new(config: CspRewriteConfig) -> Self {
        Self {
            config: Arc::new(ArcSwap::from_pointee(config)),
        }
    }

    /// Wrap an existing `ArcSwap` so callers that already share a handle can
    /// hand the same one to the stage — useful when the operator's TOML
    /// loader owns the canonical swap.
    pub fn from_swap(config: Arc<ArcSwap<CspRewriteConfig>>) -> Self {
        Self { config }
    }

    /// The shared `ArcSwap`, so external code can hot-reload the config.
    pub fn config(&self) -> Arc<ArcSwap<CspRewriteConfig>> {
        self.config.clone()
    }
}

#[async_trait]
impl ResponseStage for CspRewritter {
    async fn process(&self, mut res: Response, _state: ProxyState) -> anyhow::Result<Response> {
        let cfg = self.config.load();
        match &mut res {
            Response::Mcp(_, JsonRpcResult::Response(r)) => {
                if let Some(result) = r.result.as_mut() {
                    rewrite_result(result, &cfg);
                }
            }
            Response::McpBatch(_, items) => {
                for item in items {
                    if let JsonRpcResult::Response(r) = item
                        && let Some(result) = r.result.as_mut()
                    {
                        rewrite_result(result, &cfg);
                    }
                }
            }
            _ => {}
        }
        Ok(res)
    }
}

/// Dispatch on result shape and rewrite each widget meta site, then run a
/// deep-scan safety net that prepends the proxy URL to any CSP-shaped array
/// the targeted walks missed.
fn rewrite_result(result: &mut Value, config: &CspRewriteConfig) {
    if let Some(tools) = result.get_mut("tools").and_then(|t| t.as_array_mut()) {
        for tool in tools {
            if let Some(meta) = tool.get_mut("_meta") {
                rewrite_widget_meta(meta, None, config);
            }
        }
    } else if let Some(resources) = result.get_mut("resources").and_then(|r| r.as_array_mut()) {
        for resource in resources {
            rewrite_uri_keyed_meta(resource, "uri", config);
        }
    } else if let Some(templates) = result
        .get_mut("resourceTemplates")
        .and_then(|t| t.as_array_mut())
    {
        for template in templates {
            rewrite_uri_keyed_meta(template, "uriTemplate", config);
        }
    } else if let Some(contents) = result.get_mut("contents").and_then(|c| c.as_array_mut()) {
        for content in contents {
            rewrite_uri_keyed_meta(content, "uri", config);
        }
    } else if let Some(meta) = result.get_mut("_meta") {
        // tools/call shape: a single `_meta` lives at the top of `result`.
        rewrite_widget_meta(meta, None, config);
    }

    let _ = inject_proxy_into_all_csp(result, config);
}

/// Synthesise `_meta` when the URI alone marks a container as a widget
/// resource — under-declaring upstream servers are common, and without this
/// declared CSP would silently never apply.
fn rewrite_uri_keyed_meta(container: &mut Value, uri_key: &str, config: &CspRewriteConfig) {
    let uri = container
        .get(uri_key)
        .and_then(|v| v.as_str())
        .map(String::from);
    let has_existing_meta = container.get("_meta").is_some();
    if (uri.is_some() || has_existing_meta)
        && let Some(meta) = ensure_meta(container)
    {
        rewrite_widget_meta(meta, uri.as_deref(), config);
    }
}

/// Rewrite a widget metadata object, emitting both `openai/widgetCSP` and
/// `ui.csp` shapes from the same merge so every host sees the same effective
/// CSP. Non-widget metas are left alone (apart from a deep-scan pass that
/// repairs stray CSP arrays).
fn rewrite_widget_meta(meta: &mut Value, explicit_uri: Option<&str>, config: &CspRewriteConfig) {
    if !is_widget_meta(meta, explicit_uri) {
        let _ = inject_proxy_into_all_csp(meta, config);
        return;
    }

    // Operator's `csp.domain` flows into `openai/widgetDomain` only.
    // `_meta.ui.domain` is left untouched: Claude validates it against a hash
    // it derives from the proxy URL itself, so any value an MCP layer supplies
    // is guaranteed to fail. The empty-domain guard handles local-only dev,
    // where writing `localhost` is never useful.
    if !config.proxy_domain.is_empty() {
        write_widget_domain(meta, &config.proxy_domain);
    }

    let inferred = explicit_uri
        .map(String::from)
        .or_else(|| extract_resource_uri(meta));
    let uri = inferred.as_deref();
    let upstream_host = strip_scheme(&config.mcp_upstream);

    let connect = merged_domains(meta, Directive::Connect, uri, &upstream_host, config);
    let resource = merged_domains(meta, Directive::Resource, uri, &upstream_host, config);
    let frame = merged_domains(meta, Directive::Frame, uri, &upstream_host, config);

    write_openai_csp(meta, &connect, &resource, &frame);
    write_spec_csp(meta, &connect, &resource, &frame);

    let _ = inject_proxy_into_all_csp(meta, config);
}

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

fn extract_resource_uri(meta: &Value) -> Option<String> {
    if let Some(u) = meta.pointer("/ui/resourceUri").and_then(|v| v.as_str()) {
        return Some(u.to_string());
    }
    meta.get("openai/outputTemplate")
        .and_then(|v| v.as_str())
        .map(String::from)
}

fn merged_domains(
    meta: &Value,
    directive: Directive,
    resource_uri: Option<&str>,
    upstream_host: &str,
    config: &CspRewriteConfig,
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

/// Union of upstream-declared domains across both CSP shapes — a server that
/// only declared `ui.csp` still informs the merge for `openai/widgetCSP`
/// output and vice versa.
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

fn write_widget_domain(meta: &mut Value, domain: &str) {
    let Some(obj) = meta.as_object_mut() else {
        return;
    };
    obj.insert(
        "openai/widgetDomain".to_string(),
        Value::String(domain.to_string()),
    );
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

fn write_spec_csp(meta: &mut Value, connect: &[String], resource: &[String], frame: &[String]) {
    let Some(obj) = meta.as_object_mut() else {
        return;
    };
    let ui = obj
        .entry("ui".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    if !ui.is_object() {
        *ui = Value::Object(Map::new());
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

/// Walk `value` and prepend `config.proxy_url` to every CSP-shaped
/// `connect_domains` / `resource_domains` array that doesn't already contain
/// it. Frame arrays are skipped — see [`crate::csp`] for why.
#[must_use]
fn inject_proxy_into_all_csp(value: &mut Value, config: &CspRewriteConfig) -> bool {
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

/// Get `container._meta`, inserting an empty object if absent. `None` only
/// when the container isn't a JSON object — MCP resource entries always are,
/// so callers can treat `None` as a malformed upstream and skip.
fn ensure_meta(container: &mut Value) -> Option<&mut Value> {
    let obj = container.as_object_mut()?;
    Some(
        obj.entry("_meta".to_string())
            .or_insert_with(|| Value::Object(Map::new())),
    )
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;
    use crate::{
        protocol::mcp::{JsonRpcError, JsonRpcResponse, JsonRpcVersion, RequestId},
        proxy2::csp::{CspConfig, DirectivePolicy, Mode, WidgetScoped},
        proxy2::state::InnerProxyState,
    };
    use axum::http::response::Parts as ResponseParts;
    use bytes::Bytes;
    use serde_json::json;

    // ── Helpers ───────────────────────────────────────────────

    fn config() -> CspRewriteConfig {
        CspRewriteConfig {
            proxy_url: "https://proxy.example.com".into(),
            proxy_domain: "proxy.example.com".into(),
            mcp_upstream: "http://localhost:9000".into(),
            csp: CspConfig::default(),
        }
    }

    fn local_only_config() -> CspRewriteConfig {
        CspRewriteConfig {
            proxy_url: "http://localhost:9002".into(),
            proxy_domain: String::new(),
            mcp_upstream: "http://localhost:9000".into(),
            csp: CspConfig::default(),
        }
    }

    fn proxy_config_with_csp(csp: CspConfig) -> ProxyConfig {
        ProxyConfig {
            name: "test".into(),
            mcp: "http://upstream.internal:9000".into(),
            port: Some(9002),
            csp,
            max_request_body_size: None,
            max_response_body_size: None,
            max_concurrent_upstream: None,
            connect_timeout: None,
            request_timeout: None,
        }
    }

    fn state() -> ProxyState {
        InnerProxyState::for_tests()
    }

    fn empty_response_parts() -> ResponseParts {
        axum::http::Response::new(()).into_parts().0
    }

    fn mcp_with_result(result: Value) -> Response {
        Response::Mcp(
            empty_response_parts(),
            JsonRpcResult::Response(JsonRpcResponse {
                jsonrpc: JsonRpcVersion,
                id: RequestId::Number(1),
                result: Some(result),
            }),
        )
    }

    fn mcp_with_no_result() -> Response {
        Response::Mcp(
            empty_response_parts(),
            JsonRpcResult::Response(JsonRpcResponse {
                jsonrpc: JsonRpcVersion,
                id: RequestId::Number(1),
                result: None,
            }),
        )
    }

    fn extract_result(resp: &Response) -> &Value {
        match resp {
            Response::Mcp(_, JsonRpcResult::Response(r)) => r.result.as_ref().unwrap(),
            _ => panic!("expected Mcp response with result"),
        }
    }

    fn as_strs(arr: &Value) -> Vec<&str> {
        arr.as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect()
    }

    // ── from_proxy_config ─────────────────────────────────────

    #[test]
    fn from_proxy_config__domain_set_builds_https_url() {
        let mut csp = CspConfig::default();
        csp.domain = Some("widgets.example.com".into());
        let cfg = CspRewriteConfig::from_proxy_config(&proxy_config_with_csp(csp));
        assert_eq!(cfg.proxy_domain, "widgets.example.com");
        assert_eq!(cfg.proxy_url, "https://widgets.example.com");
        assert_eq!(cfg.mcp_upstream, "http://upstream.internal:9000");
    }

    #[test]
    fn from_proxy_config__domain_unset_signals_local_only() {
        // Empty proxy_domain + loopback proxy_url is the "no public origin"
        // signal — `is_public_proxy_origin` returns false for it, so widget
        // CSP injection is suppressed instead of leaking localhost.
        let cfg = CspRewriteConfig::from_proxy_config(&proxy_config_with_csp(CspConfig::default()));
        assert_eq!(cfg.proxy_domain, "");
        assert!(!is_public_proxy_origin(&cfg.proxy_url));
    }

    #[test]
    fn from_proxy_config__copies_csp_policies() {
        let mut csp = CspConfig::default();
        csp.domain = Some("widgets.example.com".into());
        csp.connect_domains = DirectivePolicy {
            domains: vec!["https://api.example.com".into()],
            mode: Mode::Replace,
        };
        csp.widgets.push(WidgetScoped {
            match_pattern: "ui://widget/x".into(),
            ..Default::default()
        });
        let cfg = CspRewriteConfig::from_proxy_config(&proxy_config_with_csp(csp));
        assert_eq!(cfg.csp.connect_domains.mode, Mode::Replace);
        assert_eq!(
            cfg.csp.connect_domains.domains,
            vec!["https://api.example.com"]
        );
        assert_eq!(cfg.csp.widgets.len(), 1);
    }

    // ── Pass-through cases ────────────────────────────────────

    #[tokio::test]
    async fn process__http_response_passes_through() {
        let stage = CspRewritter::new(config());
        let http = axum::http::Response::builder()
            .header("content-type", "text/html")
            .body(Bytes::from_static(b"<html/>"))
            .unwrap();
        let out = stage.process(Response::Http(http), state()).await.unwrap();
        let Response::Http(resp) = out else {
            panic!("expected Http");
        };
        assert_eq!(resp.body().as_ref(), b"<html/>");
    }

    #[tokio::test]
    async fn process__error_result_passes_through() {
        let stage = CspRewritter::new(config());
        let resp = Response::Mcp(
            empty_response_parts(),
            JsonRpcResult::Error(JsonRpcError {
                code: -32603,
                message: "boom".into(),
                data: None,
            }),
        );
        let out = stage.process(resp, state()).await.unwrap();
        let Response::Mcp(_, JsonRpcResult::Error(e)) = out else {
            panic!("expected error");
        };
        assert_eq!(e.code, -32603);
    }

    #[tokio::test]
    async fn process__missing_result_is_left_none() {
        let stage = CspRewritter::new(config());
        let out = stage.process(mcp_with_no_result(), state()).await.unwrap();
        let Response::Mcp(_, JsonRpcResult::Response(r)) = out else {
            panic!("expected Response");
        };
        assert!(r.result.is_none());
    }

    // ── tools/list shape ──────────────────────────────────────

    #[tokio::test]
    async fn process__tools_list_rewrites_widget_meta() {
        let stage = CspRewritter::new(config());
        let resp = mcp_with_result(json!({
            "tools": [{
                "name": "search",
                "_meta": {
                    "openai/widgetDomain": "old.example.com",
                    "openai/widgetCSP": {
                        "connect_domains": ["http://localhost:9000", "https://api.external.com"]
                    }
                }
            }]
        }));

        let out = stage.process(resp, state()).await.unwrap();
        let meta = &extract_result(&out)["tools"][0]["_meta"];
        assert_eq!(
            meta["openai/widgetDomain"].as_str().unwrap(),
            "proxy.example.com"
        );
        let connect = as_strs(&meta["openai/widgetCSP"]["connect_domains"]);
        assert!(connect.contains(&"https://proxy.example.com"));
        assert!(connect.contains(&"https://api.external.com"));
        assert!(!connect.iter().any(|d| d.contains("localhost")));
    }

    #[tokio::test]
    async fn process__tools_list_emits_both_csp_shapes() {
        let stage = CspRewritter::new(config());
        let resp = mcp_with_result(json!({
            "tools": [{
                "name": "search",
                "_meta": {
                    "ui": { "csp": { "connectDomains": ["https://api.spec.com"] } }
                }
            }]
        }));

        let out = stage.process(resp, state()).await.unwrap();
        let meta = &extract_result(&out)["tools"][0]["_meta"];
        let oa = as_strs(&meta["openai/widgetCSP"]["connect_domains"]);
        let spec = as_strs(&meta["ui"]["csp"]["connectDomains"]);
        assert_eq!(oa, spec);
        assert!(oa.contains(&"https://api.spec.com"));
        assert!(oa.contains(&"https://proxy.example.com"));
    }

    // ── tools/call shape ──────────────────────────────────────

    #[tokio::test]
    async fn process__tools_call_rewrites_top_level_widget_meta() {
        let stage = CspRewritter::new(config());
        let resp = mcp_with_result(json!({
            "content": [{"type": "text", "text": "result"}],
            "_meta": {
                "openai/widgetDomain": "old.example.com",
                "openai/widgetCSP": { "connect_domains": ["http://localhost:9000"] }
            }
        }));

        let out = stage.process(resp, state()).await.unwrap();
        let meta = &extract_result(&out)["_meta"];
        assert_eq!(
            meta["openai/widgetDomain"].as_str().unwrap(),
            "proxy.example.com"
        );
        let connect = as_strs(&meta["openai/widgetCSP"]["connect_domains"]);
        assert!(!connect.iter().any(|d| d.contains("localhost")));
    }

    #[tokio::test]
    async fn process__tools_call_non_widget_meta_is_not_polluted() {
        let stage = CspRewritter::new(config());
        let resp = mcp_with_result(json!({
            "content": [{"type": "text", "text": "result"}],
            "_meta": { "requestId": "abc-123" }
        }));

        let out = stage.process(resp, state()).await.unwrap();
        let meta = &extract_result(&out)["_meta"];
        assert!(meta.get("openai/widgetCSP").is_none());
        assert!(meta.get("openai/widgetDomain").is_none());
        assert_eq!(meta["requestId"].as_str().unwrap(), "abc-123");
    }

    // ── resources/list shape ─────────────────────────────────

    #[tokio::test]
    async fn process__resources_list_synthesizes_meta_from_uri() {
        let mut cfg = config();
        cfg.csp.connect_domains = DirectivePolicy {
            domains: vec!["https://api.declared.com".into()],
            mode: Mode::Replace,
        };
        let stage = CspRewritter::new(cfg);
        let resp = mcp_with_result(json!({
            "resources": [{
                "uri": "ui://widget/search",
                "name": "Search Widget"
            }]
        }));

        let out = stage.process(resp, state()).await.unwrap();
        let meta = &extract_result(&out)["resources"][0]["_meta"];
        let connect = as_strs(&meta["openai/widgetCSP"]["connect_domains"]);
        assert_eq!(
            connect,
            vec!["https://proxy.example.com", "https://api.declared.com"]
        );
    }

    #[tokio::test]
    async fn process__resources_list_widget_scope_matches_uri() {
        let mut cfg = config();
        cfg.csp.widgets.push(WidgetScoped {
            match_pattern: "ui://widget/payment*".into(),
            connect_domains: vec!["https://api.stripe.com".into()],
            connect_domains_mode: Mode::Extend,
            ..Default::default()
        });
        let stage = CspRewritter::new(cfg);
        let resp = mcp_with_result(json!({
            "resources": [
                {
                    "uri": "ui://widget/payment-form",
                    "_meta": { "openai/widgetCSP": { "connect_domains": [] } }
                },
                {
                    "uri": "ui://widget/search",
                    "_meta": { "openai/widgetCSP": { "connect_domains": [] } }
                }
            ]
        }));

        let out = stage.process(resp, state()).await.unwrap();
        let result = extract_result(&out);
        let pay = as_strs(&result["resources"][0]["_meta"]["openai/widgetCSP"]["connect_domains"]);
        let search =
            as_strs(&result["resources"][1]["_meta"]["openai/widgetCSP"]["connect_domains"]);
        assert!(pay.contains(&"https://api.stripe.com"));
        assert!(!search.contains(&"https://api.stripe.com"));
    }

    #[tokio::test]
    async fn process__resources_list_skips_when_no_uri_and_no_meta() {
        let stage = CspRewritter::new(config());
        let resp = mcp_with_result(json!({
            "resources": [{ "name": "malformed" }]
        }));

        let out = stage.process(resp, state()).await.unwrap();
        assert!(extract_result(&out)["resources"][0].get("_meta").is_none());
    }

    // ── resources/templates/list shape ───────────────────────

    #[tokio::test]
    async fn process__resources_templates_list_rewrites_widget_meta() {
        let stage = CspRewritter::new(config());
        let resp = mcp_with_result(json!({
            "resourceTemplates": [{
                "uriTemplate": "ui://widget/{name}.html",
                "_meta": { "openai/widgetCSP": { "resource_domains": ["http://localhost:4444"] } }
            }]
        }));

        let out = stage.process(resp, state()).await.unwrap();
        let resources = as_strs(
            &extract_result(&out)["resourceTemplates"][0]["_meta"]["openai/widgetCSP"]["resource_domains"],
        );
        assert!(resources.contains(&"https://proxy.example.com"));
        assert!(!resources.iter().any(|d| d.contains("localhost")));
    }

    // ── resources/read shape ─────────────────────────────────

    #[tokio::test]
    async fn process__resources_read_preserves_html_text() {
        let stage = CspRewritter::new(config());
        let resp = mcp_with_result(json!({
            "contents": [{
                "uri": "ui://widget/q",
                "mimeType": "text/html",
                "text": "<html><body>Hello</body></html>",
                "_meta": { "openai/widgetDomain": "old.example.com" }
            }]
        }));

        let out = stage.process(resp, state()).await.unwrap();
        let entry = &extract_result(&out)["contents"][0];
        assert_eq!(
            entry["text"].as_str().unwrap(),
            "<html><body>Hello</body></html>"
        );
        assert_eq!(
            entry["_meta"]["openai/widgetDomain"].as_str().unwrap(),
            "proxy.example.com"
        );
    }

    // ── Frame directive ──────────────────────────────────────

    #[tokio::test]
    async fn process__frame_domains_omits_proxy_url() {
        let mut cfg = config();
        cfg.csp.frame_domains = DirectivePolicy {
            domains: vec!["https://embed.partner.com".into()],
            mode: Mode::Extend,
        };
        let stage = CspRewritter::new(cfg);
        let resp = mcp_with_result(json!({
            "tools": [{
                "name": "x",
                "_meta": { "openai/widgetCSP": { "frame_domains": [] } }
            }]
        }));

        let out = stage.process(resp, state()).await.unwrap();
        let frames = as_strs(
            &extract_result(&out)["tools"][0]["_meta"]["openai/widgetCSP"]["frame_domains"],
        );
        assert_eq!(frames, vec!["https://embed.partner.com"]);
    }

    // ── Local-only mode ──────────────────────────────────────

    #[tokio::test]
    async fn process__local_only_skips_proxy_injection() {
        let stage = CspRewritter::new(local_only_config());
        let resp = mcp_with_result(json!({
            "contents": [{
                "uri": "ui://widget/x",
                "_meta": {
                    "openai/widgetCSP": { "connect_domains": ["https://api.external.com"] }
                }
            }]
        }));

        let out = stage.process(resp, state()).await.unwrap();
        let meta = &extract_result(&out)["contents"][0]["_meta"];
        let connect = as_strs(&meta["openai/widgetCSP"]["connect_domains"]);
        assert_eq!(connect, vec!["https://api.external.com"]);
        assert!(meta.get("openai/widgetDomain").is_none());
    }

    // ── McpBatch ─────────────────────────────────────────────

    #[tokio::test]
    async fn process__mcp_batch_rewrites_each_response() {
        let stage = CspRewritter::new(config());
        let make = |id: i64| {
            JsonRpcResult::Response(JsonRpcResponse {
                jsonrpc: JsonRpcVersion,
                id: RequestId::Number(id),
                result: Some(json!({
                    "tools": [{
                        "name": "t",
                        "_meta": { "openai/widgetCSP": { "connect_domains": [] } }
                    }]
                })),
            })
        };
        let resp = Response::McpBatch(empty_response_parts(), vec![make(1), make(2)]);

        let out = stage.process(resp, state()).await.unwrap();
        let Response::McpBatch(_, items) = out else {
            panic!("expected McpBatch");
        };
        assert_eq!(items.len(), 2);
        for item in &items {
            let JsonRpcResult::Response(r) = item else {
                panic!("expected Response");
            };
            let connect = as_strs(
                &r.result.as_ref().unwrap()["tools"][0]["_meta"]["openai/widgetCSP"]["connect_domains"],
            );
            assert!(connect.contains(&"https://proxy.example.com"));
        }
    }

    #[tokio::test]
    async fn process__mcp_batch_skips_error_items() {
        let stage = CspRewritter::new(config());
        let ok = JsonRpcResult::Response(JsonRpcResponse {
            jsonrpc: JsonRpcVersion,
            id: RequestId::Number(1),
            result: Some(json!({
                "tools": [{ "name": "t", "_meta": { "openai/widgetCSP": { "connect_domains": [] } } }]
            })),
        });
        let err = JsonRpcResult::Error(JsonRpcError {
            code: -32603,
            message: "boom".into(),
            data: None,
        });
        let resp = Response::McpBatch(empty_response_parts(), vec![ok, err]);

        let out = stage.process(resp, state()).await.unwrap();
        let Response::McpBatch(_, items) = out else {
            panic!();
        };
        assert!(matches!(items[0], JsonRpcResult::Response(_)));
        assert!(matches!(items[1], JsonRpcResult::Error(_)));
    }

    // ── Hot reload ───────────────────────────────────────────

    #[tokio::test]
    async fn process__arc_swap_hot_reload_uses_new_config() {
        let stage = CspRewritter::new(config());
        stage.config().store(Arc::new(CspRewriteConfig {
            proxy_url: "https://v2.example.com".into(),
            proxy_domain: "v2.example.com".into(),
            mcp_upstream: "http://localhost:9000".into(),
            csp: CspConfig::default(),
        }));

        let resp = mcp_with_result(json!({
            "tools": [{
                "name": "x",
                "_meta": { "openai/widgetCSP": { "connect_domains": [] } }
            }]
        }));
        let out = stage.process(resp, state()).await.unwrap();
        let connect = as_strs(
            &extract_result(&out)["tools"][0]["_meta"]["openai/widgetCSP"]["connect_domains"],
        );
        assert!(connect.contains(&"https://v2.example.com"));
        assert!(!connect.iter().any(|d| d.contains("proxy.example.com")));
    }

    #[tokio::test]
    async fn process__from_swap_shares_handle_with_caller() {
        let swap = Arc::new(ArcSwap::from_pointee(config()));
        let stage = CspRewritter::from_swap(swap.clone());
        swap.store(Arc::new(CspRewriteConfig {
            proxy_url: "https://shared.example.com".into(),
            proxy_domain: "shared.example.com".into(),
            mcp_upstream: "http://localhost:9000".into(),
            csp: CspConfig::default(),
        }));

        let resp = mcp_with_result(json!({
            "tools": [{
                "name": "x",
                "_meta": { "openai/widgetCSP": { "connect_domains": [] } }
            }]
        }));
        let out = stage.process(resp, state()).await.unwrap();
        let connect = as_strs(
            &extract_result(&out)["tools"][0]["_meta"]["openai/widgetCSP"]["connect_domains"],
        );
        assert!(connect.contains(&"https://shared.example.com"));
    }

    // ── Deep-scan safety net ─────────────────────────────────

    #[tokio::test]
    async fn process__deep_scan_injects_proxy_into_nested_csp() {
        let stage = CspRewritter::new(config());
        let resp = mcp_with_result(json!({
            "deeply": {
                "nested": { "connect_domains": ["https://only-external.com"] }
            }
        }));

        let out = stage.process(resp, state()).await.unwrap();
        let domains = as_strs(&extract_result(&out)["deeply"]["nested"]["connect_domains"]);
        assert_eq!(
            domains,
            vec!["https://proxy.example.com", "https://only-external.com"]
        );
    }

    #[tokio::test]
    async fn process__deep_scan_skips_frame_arrays() {
        let stage = CspRewritter::new(config());
        let resp = mcp_with_result(json!({
            "tools": [{
                "name": "x",
                "_meta": {
                    "deeply": { "frame_domains": ["https://embed.partner.com"] }
                }
            }]
        }));

        let out = stage.process(resp, state()).await.unwrap();
        let frames = as_strs(&extract_result(&out)["tools"][0]["_meta"]["deeply"]["frame_domains"]);
        assert_eq!(frames, vec!["https://embed.partner.com"]);
    }
}
