//! Proxy TOML configuration — file shape and resolved runtime form.
//!
//! Two layers:
//! - [`FileProxyConfig`] (and friends) deserialize directly from `mcpr.toml`.
//!   Permissive: optional fields, legacy spellings, sub-tables.
//! - [`ProxyConfig`] is the resolved form the runtime consumes. Defaults are
//!   applied, modes are parsed into enums, and the proxy name is derived.
//!
//! Conversion goes through [`FileProxyConfig::resolve`].

use serde::Deserialize;
use std::path::Path;

use crate::csp::{CspConfig, DirectivePolicy, Mode as CspMode, WidgetScoped};

// ── File shape ─────────────────────────────────────────────────────────

/// Top-level proxy section of `mcpr.toml`.
#[derive(Deserialize)]
pub struct FileProxyConfig {
    /// Upstream MCP server URL. Required — without it, the proxy has nothing
    /// to forward to.
    pub mcp: String,

    /// Proxy identity. Falls back to filename stem, then `"default"`.
    pub name: Option<String>,
    /// Bind port. `None` lets the OS choose.
    pub port: Option<u16>,

    #[serde(default)]
    pub csp: FileCspConfig,

    pub max_request_body_size: Option<usize>,
    pub max_response_body_size: Option<usize>,
    pub max_concurrent_upstream: Option<usize>,
    pub connect_timeout: Option<u64>,
    pub request_timeout: Option<u64>,
}

/// `[csp]` table.
///
/// Canonical shape is one sub-table per directive plus an optional
/// `[[csp.widget]]` array:
///
/// ```toml
/// [csp.connectDomains]
/// domains = ["api.example.com"]
/// mode    = "extend"
///
/// [csp.resourceDomains]
/// domains = ["cdn.example.com"]
/// mode    = "extend"
///
/// [csp.frameDomains]
/// domains = []
/// mode    = "replace"
///
/// [[csp.widget]]
/// match              = "ui://widget/payment*"
/// connectDomains     = ["api.stripe.com"]
/// connectDomainsMode = "extend"
/// ```
///
/// The legacy flat shape (`csp.mode` + `csp.domains`) is still accepted for
/// one release and folded into `connectDomains` and `resourceDomains`.
#[derive(Deserialize, Default)]
#[serde(default)]
pub struct FileCspConfig {
    // -- Legacy flat shape (deprecated) --
    pub mode: Option<String>,
    pub domains: Vec<String>,

    // -- Canonical per-directive shape --
    #[serde(rename = "connectDomains")]
    pub connect_domains: Option<FileDirectivePolicy>,
    #[serde(rename = "resourceDomains")]
    pub resource_domains: Option<FileDirectivePolicy>,
    #[serde(rename = "frameDomains")]
    pub frame_domains: Option<FileDirectivePolicy>,

    /// Bare public host (no scheme) for this proxy. Feeds the
    /// `openai/widgetDomain` meta field and the proxy-URL CSP injection.
    /// `_meta.ui.domain` is left untouched — Claude derives that field itself
    /// and rejects any value supplied by an MCP layer. When unset, the runtime
    /// falls back to the tunnel URL or suppresses injection in local-only mode
    /// rather than leaking `localhost` into widget config.
    pub domain: Option<String>,

    #[serde(rename = "widget")]
    pub widgets: Vec<WidgetScoped>,
}

/// Per-directive policy as it appears in the config file.
#[derive(Deserialize, Default)]
#[serde(default)]
pub struct FileDirectivePolicy {
    pub domains: Vec<String>,
    pub mode: Option<String>,
}

// ── Mode parsing ───────────────────────────────────────────────────────

/// Parse a CSP merge mode string. Unknown values return `None` so callers
/// can surface the error during validation.
pub fn parse_mode(s: &str) -> Option<CspMode> {
    match s.to_lowercase().as_str() {
        "extend" => Some(CspMode::Extend),
        "replace" | "override" => Some(CspMode::Replace),
        _ => None,
    }
}

impl FileDirectivePolicy {
    pub fn into_policy(self, default_mode: CspMode) -> DirectivePolicy {
        let mode = self
            .mode
            .as_deref()
            .and_then(parse_mode)
            .unwrap_or(default_mode);
        DirectivePolicy {
            domains: self.domains,
            mode,
        }
    }
}

impl FileCspConfig {
    /// Lower the file representation into a runtime [`CspConfig`].
    ///
    /// Precedence:
    /// - Per-directive blocks (`connectDomains` etc.) fully define their directive.
    /// - Otherwise the legacy `mode` + `domains` pair fills both `connectDomains`
    ///   and `resourceDomains` with the same values.
    /// - `frameDomains` defaults to `replace` with an empty list.
    pub fn into_runtime(self) -> CspConfig {
        let legacy_mode = self
            .mode
            .as_deref()
            .and_then(parse_mode)
            .unwrap_or(CspMode::Extend);
        let legacy_domains = self.domains;

        let connect = match self.connect_domains {
            Some(p) => p.into_policy(CspMode::Extend),
            None => DirectivePolicy {
                domains: legacy_domains.clone(),
                mode: legacy_mode,
            },
        };
        let resource = match self.resource_domains {
            Some(p) => p.into_policy(CspMode::Extend),
            None => DirectivePolicy {
                domains: legacy_domains,
                mode: legacy_mode,
            },
        };
        let frame = match self.frame_domains {
            Some(p) => p.into_policy(CspMode::Replace),
            None => DirectivePolicy::strict(),
        };

        CspConfig {
            connect_domains: connect,
            resource_domains: resource,
            frame_domains: frame,
            widgets: self.widgets,
            domain: self
                .domain
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
        }
    }
}

// ── Resolved runtime form ──────────────────────────────────────────────

/// Resolved proxy configuration consumed by the runtime.
#[derive(Clone)]
pub struct ProxyConfig {
    pub name: String,
    pub mcp: String,
    pub port: Option<u16>,
    pub csp: CspConfig,

    pub max_request_body_size: Option<usize>,
    pub max_response_body_size: Option<usize>,
    pub max_concurrent_upstream: Option<usize>,
    pub connect_timeout: Option<u64>,
    pub request_timeout: Option<u64>,
}

impl FileProxyConfig {
    /// Lower the file form into a runtime [`ProxyConfig`].
    ///
    /// `config_path` is used to derive the proxy name when `name` is unset
    /// (filename stem; `mcpr.toml` becomes `default`).
    pub fn resolve(self, config_path: Option<&Path>) -> ProxyConfig {
        let name = resolve_proxy_name(self.name.as_deref(), config_path);
        ProxyConfig {
            name,
            mcp: self.mcp,
            port: self.port,
            csp: self.csp.into_runtime(),
            max_request_body_size: self.max_request_body_size,
            max_response_body_size: self.max_response_body_size,
            max_concurrent_upstream: self.max_concurrent_upstream,
            connect_timeout: self.connect_timeout,
            request_timeout: self.request_timeout,
        }
    }
}

impl ProxyConfig {
    /// Names of fields that differ between `self` and `other` and require a
    /// full restart rather than a hot reload.
    ///
    /// `csp` is intentionally omitted — it is the only field a live reload may
    /// swap. `name` is omitted too: a proxy's identity is fixed at boot.
    pub fn reload_unsafe_changes(&self, other: &ProxyConfig) -> Vec<&'static str> {
        let mut changed = Vec::new();
        if self.mcp != other.mcp {
            changed.push("mcp");
        }
        if self.port != other.port {
            changed.push("port");
        }
        if self.max_request_body_size != other.max_request_body_size {
            changed.push("max_request_body_size");
        }
        if self.max_response_body_size != other.max_response_body_size {
            changed.push("max_response_body_size");
        }
        if self.max_concurrent_upstream != other.max_concurrent_upstream {
            changed.push("max_concurrent_upstream");
        }
        if self.connect_timeout != other.connect_timeout {
            changed.push("connect_timeout");
        }
        if self.request_timeout != other.request_timeout {
            changed.push("request_timeout");
        }
        changed
    }
}

// ── Identity resolution ────────────────────────────────────────────────

/// Resolve proxy name: explicit > filename stem (with `mcpr.toml` → `default`)
/// > `default`. Characters that aren't `[A-Za-z0-9-]` are sanitized to `-`.
fn resolve_proxy_name(explicit_name: Option<&str>, config_path: Option<&Path>) -> String {
    let raw = match explicit_name {
        Some(n) => n.to_string(),
        None => config_path
            .and_then(|p| p.file_stem())
            .and_then(|s| s.to_str())
            .map(|s| if s == "mcpr" { "default" } else { s })
            .unwrap_or("default")
            .to_string(),
    };

    raw.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;

    fn proxy_config() -> ProxyConfig {
        ProxyConfig {
            name: "test".into(),
            mcp: "http://localhost:9000".into(),
            port: Some(3000),
            csp: CspConfig::default(),
            max_request_body_size: None,
            max_response_body_size: None,
            max_concurrent_upstream: None,
            connect_timeout: None,
            request_timeout: None,
        }
    }

    // ── reload_unsafe_changes ──────────────────────────────────────────

    #[test]
    fn reload_unsafe_changes__identical_is_empty() {
        let a = proxy_config();
        let b = proxy_config();
        assert!(a.reload_unsafe_changes(&b).is_empty());
    }

    #[test]
    fn reload_unsafe_changes__csp_difference_is_safe() {
        let a = proxy_config();
        let mut b = proxy_config();
        b.csp.widgets.push(WidgetScoped {
            match_pattern: "ui://widget/test".into(),
            ..Default::default()
        });
        assert!(a.reload_unsafe_changes(&b).is_empty());
    }

    #[test]
    fn reload_unsafe_changes__name_difference_is_safe() {
        let a = proxy_config();
        let mut b = proxy_config();
        b.name = "renamed".into();
        assert!(a.reload_unsafe_changes(&b).is_empty());
    }

    #[test]
    fn reload_unsafe_changes__mcp_flagged() {
        let a = proxy_config();
        let mut b = proxy_config();
        b.mcp = "http://localhost:9999".into();
        assert_eq!(a.reload_unsafe_changes(&b), vec!["mcp"]);
    }

    #[test]
    fn reload_unsafe_changes__port_flagged() {
        let a = proxy_config();
        let mut b = proxy_config();
        b.port = Some(4000);
        assert_eq!(a.reload_unsafe_changes(&b), vec!["port"]);
    }

    #[test]
    fn reload_unsafe_changes__multiple_fields_listed() {
        let a = proxy_config();
        let mut b = proxy_config();
        b.mcp = "http://other:9000".into();
        b.port = Some(4000);
        let changed = a.reload_unsafe_changes(&b);
        assert!(changed.contains(&"mcp"));
        assert!(changed.contains(&"port"));
    }

    // ── File deserialization ──────────────────────────────────────────

    #[test]
    fn file_proxy_config__max_request_body_size() {
        let toml_str = r#"
            mcp = "http://localhost:9000"
            port = 8080
            max_request_body_size = 10485760
        "#;
        let config: FileProxyConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.max_request_body_size, Some(10_485_760));
    }

    #[test]
    fn file_proxy_config__max_response_body_size() {
        let toml_str = r#"
            mcp = "http://localhost:9000"
            port = 8080
            max_response_body_size = 20971520
        "#;
        let config: FileProxyConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.max_response_body_size, Some(20_971_520));
    }

    #[test]
    fn file_proxy_config__body_size_defaults_to_none() {
        let toml_str = r#"
            mcp = "http://localhost:9000"
            port = 8080
        "#;
        let config: FileProxyConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.max_request_body_size, None);
        assert_eq!(config.max_response_body_size, None);
        assert_eq!(config.max_concurrent_upstream, None);
    }

    #[test]
    fn file_proxy_config__max_concurrent_upstream() {
        let toml_str = r#"
            mcp = "http://localhost:9000"
            port = 8080
            max_concurrent_upstream = 50
        "#;
        let config: FileProxyConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.max_concurrent_upstream, Some(50));
    }

    #[test]
    fn file_proxy_config__name_from_toml() {
        let toml_str = r#"
            name = "email"
            mcp = "http://localhost:9000"
        "#;
        let config: FileProxyConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.name.as_deref(), Some("email"));
    }

    // ── CSP config ────────────────────────────────────────────────────

    #[test]
    fn csp_config__canonical_shape_parses() {
        let toml_str = r#"
            mcp = "http://localhost:9000"

            [csp.connectDomains]
            domains = ["api.example.com"]
            mode    = "extend"

            [csp.resourceDomains]
            domains = ["cdn.example.com"]
            mode    = "extend"

            [csp.frameDomains]
            domains = []
            mode    = "replace"

            [[csp.widget]]
            match              = "ui://widget/payment*"
            connectDomains     = ["api.stripe.com"]
            connectDomainsMode = "extend"
        "#;
        let file: FileProxyConfig = toml::from_str(toml_str).unwrap();
        let cfg = file.csp.into_runtime();
        assert_eq!(cfg.connect_domains.domains, vec!["api.example.com"]);
        assert_eq!(cfg.connect_domains.mode, CspMode::Extend);
        assert_eq!(cfg.resource_domains.domains, vec!["cdn.example.com"]);
        assert_eq!(cfg.frame_domains.mode, CspMode::Replace);
        assert_eq!(cfg.widgets.len(), 1);
        assert_eq!(cfg.widgets[0].match_pattern, "ui://widget/payment*");
        assert_eq!(cfg.widgets[0].connect_domains, vec!["api.stripe.com"]);
        assert_eq!(cfg.widgets[0].connect_domains_mode, CspMode::Extend);
    }

    #[test]
    fn csp_config__legacy_flat_shape_populates_connect_and_resource() {
        let toml_str = r#"
            mcp = "http://localhost:9000"

            [csp]
            mode    = "extend"
            domains = ["api.legacy.com"]
        "#;
        let file: FileProxyConfig = toml::from_str(toml_str).unwrap();
        let cfg = file.csp.into_runtime();
        assert_eq!(cfg.connect_domains.domains, vec!["api.legacy.com"]);
        assert_eq!(cfg.resource_domains.domains, vec!["api.legacy.com"]);
        assert_eq!(cfg.connect_domains.mode, CspMode::Extend);
        assert_eq!(cfg.frame_domains.mode, CspMode::Replace);
    }

    #[test]
    fn csp_config__legacy_override_maps_to_replace() {
        let toml_str = r#"
            mcp = "http://localhost:9000"

            [csp]
            mode    = "override"
            domains = ["api.legacy.com"]
        "#;
        let file: FileProxyConfig = toml::from_str(toml_str).unwrap();
        let cfg = file.csp.into_runtime();
        assert_eq!(cfg.connect_domains.mode, CspMode::Replace);
        assert_eq!(cfg.resource_domains.mode, CspMode::Replace);
    }

    #[test]
    fn csp_config__empty_defaults_strict_frames() {
        let file: FileProxyConfig = toml::from_str(r#"mcp = "http://localhost:9000""#).unwrap();
        let cfg = file.csp.into_runtime();
        assert!(cfg.connect_domains.domains.is_empty());
        assert!(cfg.resource_domains.domains.is_empty());
        assert_eq!(cfg.frame_domains.mode, CspMode::Replace);
        assert!(cfg.widgets.is_empty());
        assert!(cfg.domain.is_none());
    }

    #[test]
    fn csp_config__domain_parses() {
        let toml_str = r#"
            mcp = "http://localhost:9000"

            [csp]
            domain = "widgets.example.com"
        "#;
        let file: FileProxyConfig = toml::from_str(toml_str).unwrap();
        let cfg = file.csp.into_runtime();
        assert_eq!(cfg.domain.as_deref(), Some("widgets.example.com"));
    }

    #[test]
    fn csp_config__domain_whitespace_is_trimmed_and_empty_ignored() {
        let toml_str = r#"
            mcp = "http://localhost:9000"

            [csp]
            domain = "   "
        "#;
        let file: FileProxyConfig = toml::from_str(toml_str).unwrap();
        let cfg = file.csp.into_runtime();
        assert!(cfg.domain.is_none());
    }

    #[test]
    fn csp_config__canonical_overrides_legacy_when_both_present() {
        // Operator partially migrated: new connectDomains plus a leftover
        // legacy mode+domains. The new block wins; legacy fills the rest.
        let toml_str = r#"
            mcp = "http://localhost:9000"

            [csp]
            mode    = "extend"
            domains = ["legacy.com"]

            [csp.connectDomains]
            domains = ["new.com"]
            mode    = "replace"
        "#;
        let file: FileProxyConfig = toml::from_str(toml_str).unwrap();
        let cfg = file.csp.into_runtime();
        assert_eq!(cfg.connect_domains.domains, vec!["new.com"]);
        assert_eq!(cfg.connect_domains.mode, CspMode::Replace);
        assert_eq!(cfg.resource_domains.domains, vec!["legacy.com"]);
        assert_eq!(cfg.resource_domains.mode, CspMode::Extend);
    }

    #[test]
    fn parse_mode__accepts_known_values() {
        assert_eq!(parse_mode("extend"), Some(CspMode::Extend));
        assert_eq!(parse_mode("replace"), Some(CspMode::Replace));
        assert_eq!(parse_mode("override"), Some(CspMode::Replace));
        assert_eq!(parse_mode("EXTEND"), Some(CspMode::Extend));
    }

    #[test]
    fn parse_mode__rejects_unknown() {
        assert_eq!(parse_mode(""), None);
        assert_eq!(parse_mode("strict"), None);
        assert_eq!(parse_mode("off"), None);
    }

    // ── Proxy name resolution ────────────────────────────────────────

    #[test]
    fn resolve_proxy_name__explicit_wins() {
        let name = resolve_proxy_name(Some("my-proxy"), Some(Path::new("/tmp/search.toml")));
        assert_eq!(name, "my-proxy");
    }

    #[test]
    fn resolve_proxy_name__from_filename_stem() {
        let name = resolve_proxy_name(None, Some(Path::new("/tmp/search.toml")));
        assert_eq!(name, "search");
    }

    #[test]
    fn resolve_proxy_name__mcpr_toml_becomes_default() {
        let name = resolve_proxy_name(None, Some(Path::new("/tmp/mcpr.toml")));
        assert_eq!(name, "default");
    }

    #[test]
    fn resolve_proxy_name__no_config_becomes_default() {
        let name = resolve_proxy_name(None, None);
        assert_eq!(name, "default");
    }

    #[test]
    fn resolve_proxy_name__sanitizes_special_chars() {
        let name = resolve_proxy_name(Some("my proxy!@#$"), None);
        assert_eq!(name, "my-proxy----");
    }

    #[test]
    fn resolve_proxy_name__preserves_hyphens() {
        let name = resolve_proxy_name(Some("search-v2"), None);
        assert_eq!(name, "search-v2");
    }

    // ── End-to-end resolve ────────────────────────────────────────────

    #[test]
    fn resolve__name_defaults_to_default_when_no_path() {
        let file: FileProxyConfig = toml::from_str(r#"mcp = "http://localhost:9000""#).unwrap();
        let cfg = file.resolve(None);
        assert_eq!(cfg.name, "default");
    }

    #[test]
    fn resolve__name_from_explicit_field() {
        let toml_str = r#"
            name = "email"
            mcp = "http://localhost:9000"
        "#;
        let file: FileProxyConfig = toml::from_str(toml_str).unwrap();
        let cfg = file.resolve(Some(Path::new("/tmp/anything.toml")));
        assert_eq!(cfg.name, "email");
    }

    #[test]
    fn resolve__name_from_filename_when_field_unset() {
        let file: FileProxyConfig = toml::from_str(r#"mcp = "http://localhost:9000""#).unwrap();
        let cfg = file.resolve(Some(Path::new("/tmp/search.toml")));
        assert_eq!(cfg.name, "search");
    }

    #[test]
    fn file_proxy_config__missing_mcp_fails_deserialize() {
        let result = toml::from_str::<FileProxyConfig>("port = 8080");
        let err = result
            .as_ref()
            .err()
            .expect("expected deserialize error when mcp is missing");
        assert!(err.to_string().contains("mcp"), "error was: {err}");
    }
}
