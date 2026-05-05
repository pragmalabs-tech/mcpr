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

use super::csp::{CspConfig, DirectivePolicy, Mode as CspMode, WidgetScoped};
use crate::auth::ProtectedResourceMetadata;

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

    /// Optional `[auth]` block. Presence activates the
    /// `/.well-known/oauth-protected-resource` discovery endpoint and
    /// makes the proxy advertise itself as an RFC 9728 protected
    /// resource. Absence leaves the proxy purely transparent for
    /// auth-related traffic.
    pub auth: Option<FileAuthConfig>,
}

/// `[auth]` table - mcpr's own OAuth 2.1 protected-resource declaration.
///
/// When present, the proxy mounts `/.well-known/oauth-protected-resource`
/// (RFC 9728) and advertises which authorization server clients should
/// talk to. Validation of presented bearers is opt-in via concrete
/// providers in Phase 2 (JWKS, Auth0, Supabase).
#[derive(Deserialize, Default, Clone, Debug)]
#[serde(default)]
pub struct FileAuthConfig {
    /// One or more authorization server URLs. Required when `[auth]`
    /// is present.
    pub authorization_servers: Vec<String>,
    /// Defaults to `["header"]` per RFC 9728.
    pub bearer_methods_supported: Option<Vec<String>>,
    pub scopes_supported: Option<Vec<String>>,
    /// Override the resource URL advertised in metadata. Defaults to
    /// the proxy's listening origin (computed from `port`).
    pub resource: Option<String>,
    pub resource_documentation: Option<String>,
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
///
/// `Default` is derived so tests can build configs ergonomically with
/// `..Default::default()`. Adding a new field to `ProxyConfig` no
/// longer cascades into every test fixture - they pick up the default
/// automatically.
#[derive(Clone, Default)]
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

    pub auth: Option<AuthConfig>,
}

impl ProxyConfig {
    /// Sensible default for unit tests. Wraps an empty config and
    /// overrides only the upstream `mcp` URL, leaving every other
    /// field at its default. Adding fields to `ProxyConfig` does not
    /// break callers of this helper.
    pub fn for_tests(mcp: impl Into<String>) -> Self {
        Self {
            name: "test".into(),
            mcp: mcp.into(),
            ..Self::default()
        }
    }
}

/// Resolved `[auth]` configuration.
#[derive(Clone, Debug, Default)]
pub struct AuthConfig {
    pub resource: String,
    pub authorization_servers: Vec<String>,
    pub bearer_methods_supported: Vec<String>,
    pub scopes_supported: Option<Vec<String>>,
    pub resource_documentation: Option<String>,
}

impl AuthConfig {
    /// Build the RFC 9728 metadata document advertised at
    /// `/.well-known/oauth-protected-resource`.
    pub fn metadata(&self) -> ProtectedResourceMetadata {
        ProtectedResourceMetadata {
            resource: self.resource.clone(),
            authorization_servers: self.authorization_servers.clone(),
            bearer_methods_supported: self.bearer_methods_supported.clone(),
            scopes_supported: self.scopes_supported.clone(),
            resource_documentation: self.resource_documentation.clone(),
        }
    }
}

impl FileProxyConfig {
    /// Lower the file form into a runtime [`ProxyConfig`].
    ///
    /// `config_path` is used to derive the proxy name when `name` is unset
    /// (filename stem; `mcpr.toml` becomes `default`).
    pub fn resolve(self, config_path: Option<&Path>) -> ProxyConfig {
        let name = resolve_proxy_name(self.name.as_deref(), config_path);
        let port = self.port;
        let auth = self.auth.and_then(|a| a.into_runtime(port));
        ProxyConfig {
            name,
            mcp: self.mcp,
            port,
            csp: self.csp.into_runtime(),
            max_request_body_size: self.max_request_body_size,
            max_response_body_size: self.max_response_body_size,
            max_concurrent_upstream: self.max_concurrent_upstream,
            connect_timeout: self.connect_timeout,
            request_timeout: self.request_timeout,
            auth,
        }
    }
}

impl FileAuthConfig {
    /// Lower the file form into [`AuthConfig`]. Returns `None` when the
    /// block is empty enough that mounting the discovery endpoint would
    /// be misleading - specifically, when no authorization servers are
    /// declared (RFC 9728 requires at least one).
    pub fn into_runtime(self, port: Option<u16>) -> Option<AuthConfig> {
        if self.authorization_servers.is_empty() {
            return None;
        }
        let resource = self.resource.unwrap_or_else(|| {
            let p = port.unwrap_or(0);
            format!("http://localhost:{p}")
        });
        let bearer_methods_supported = self
            .bearer_methods_supported
            .unwrap_or_else(|| vec!["header".to_string()]);
        Some(AuthConfig {
            resource,
            authorization_servers: self.authorization_servers,
            bearer_methods_supported,
            scopes_supported: self.scopes_supported,
            resource_documentation: self.resource_documentation,
        })
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

    // ── [auth] block ──────────────────────────────────────────────────

    #[test]
    fn file_auth_config__optional_when_absent() {
        let toml_str = r#"mcp = "http://localhost:9000""#;
        let file: FileProxyConfig = toml::from_str(toml_str).unwrap();
        assert!(file.auth.is_none());
    }

    #[test]
    fn file_auth_config__parses_authorization_servers() {
        let toml_str = r#"
            mcp = "http://localhost:9000"
            [auth]
            authorization_servers = ["https://auth.example.com"]
        "#;
        let file: FileProxyConfig = toml::from_str(toml_str).unwrap();
        let auth = file.auth.expect("auth block");
        assert_eq!(auth.authorization_servers, vec!["https://auth.example.com"]);
    }

    #[test]
    fn auth_config__defaults_resource_from_port() {
        let file = FileAuthConfig {
            authorization_servers: vec!["https://auth.example.com".into()],
            ..Default::default()
        };
        let runtime = file.into_runtime(Some(3000)).unwrap();
        assert_eq!(runtime.resource, "http://localhost:3000");
    }

    #[test]
    fn auth_config__defaults_bearer_methods_to_header() {
        let file = FileAuthConfig {
            authorization_servers: vec!["https://auth.example.com".into()],
            ..Default::default()
        };
        let runtime = file.into_runtime(Some(3000)).unwrap();
        assert_eq!(runtime.bearer_methods_supported, vec!["header"]);
    }

    #[test]
    fn auth_config__honours_explicit_resource_override() {
        let file = FileAuthConfig {
            authorization_servers: vec!["https://auth.example.com".into()],
            resource: Some("https://api.public.example.com".into()),
            ..Default::default()
        };
        let runtime = file.into_runtime(Some(3000)).unwrap();
        assert_eq!(runtime.resource, "https://api.public.example.com");
    }

    #[test]
    fn auth_config__rejects_empty_authorization_servers() {
        let file = FileAuthConfig::default();
        let runtime = file.into_runtime(Some(3000));
        assert!(
            runtime.is_none(),
            "auth without authorization_servers should yield None"
        );
    }

    #[test]
    fn auth_config__metadata_round_trip() {
        let runtime = AuthConfig {
            resource: "http://localhost:3000".into(),
            authorization_servers: vec!["https://auth.example.com".into()],
            bearer_methods_supported: vec!["header".into()],
            scopes_supported: Some(vec!["mcp:tools".into()]),
            resource_documentation: Some("https://docs.example.com".into()),
        };
        let m = runtime.metadata();
        assert_eq!(m.resource, "http://localhost:3000");
        assert_eq!(m.authorization_servers, vec!["https://auth.example.com"]);
        assert_eq!(m.scopes_supported, Some(vec!["mcp:tools".to_string()]));
    }

    #[test]
    fn resolve__threads_auth_through() {
        let toml_str = r#"
            mcp = "http://localhost:9000"
            port = 3000

            [auth]
            authorization_servers = ["https://auth.example.com"]
            scopes_supported = ["mcp:tools", "mcp:resources"]
        "#;
        let file: FileProxyConfig = toml::from_str(toml_str).unwrap();
        let cfg = file.resolve(None);
        let auth = cfg.auth.expect("auth resolved");
        assert_eq!(auth.resource, "http://localhost:3000");
        assert_eq!(
            auth.scopes_supported,
            Some(vec!["mcp:tools".to_string(), "mcp:resources".to_string()])
        );
    }

    #[test]
    fn resolve__auth_absent_when_block_missing() {
        let toml_str = r#"mcp = "http://localhost:9000""#;
        let file: FileProxyConfig = toml::from_str(toml_str).unwrap();
        let cfg = file.resolve(None);
        assert!(cfg.auth.is_none());
    }
}
