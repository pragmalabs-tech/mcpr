//! # CSP — Declarative Content Security Policy for widgets
//!
//! This module owns the config types and the merge function that together decide
//! which domains appear in a widget's CSP when mcpr rewrites an MCP response.
//!
//! ## Model
//!
//! A widget's CSP has three independent directive arrays:
//!
//! - `connectDomains` — allowed targets for `fetch`, `WebSocket`, `EventSource`.
//! - `resourceDomains` — allowed sources for scripts, styles, images, fonts, media.
//! - `frameDomains` — allowed sources for nested `<iframe>` content.
//!
//! Each directive carries its own [`DirectivePolicy`] — a list of domains and a
//! [`Mode`] (`extend` or `replace`) that decides how to combine declared domains
//! with whatever the upstream MCP server already returned.
//!
//! A top-level [`CspConfig`] holds one policy per directive plus an optional list
//! of [`WidgetScoped`] entries. Widget entries match resource URIs with glob
//! patterns (e.g. `ui://widget/payment*`) and layer on top of the global policy.
//!
//! ## Merge
//!
//! [`effective_domains`] computes the final domain list for one directive, given
//! upstream domains, a resource URI, and the config. The rules are:
//!
//! 1. If the global directive's mode is `replace`, discard upstream entirely;
//!    otherwise start from upstream minus localhost and the upstream host itself.
//! 2. Append the global directive's declared domains.
//! 3. For each widget entry whose `match` glob matches the resource URI, in
//!    config order, either extend (append) or replace (overwrite) the working
//!    list with the widget's domains for this directive.
//! 4. For `connect` and `resource`, prepend the proxy URL and dedupe. `frame`
//!    does not receive the proxy URL — widgets don't iframe the proxy back into
//!    themselves, and prepending it would make every widget look like an
//!    iframe-embedder to hosts that flag that shape for extra review.
//!
//! Replace semantics are scoped: a global replace only ignores upstream; a
//! widget replace wipes everything accumulated above it.
//!
//! ## Example
//!
//! ```toml
//! [csp.connectDomains]
//! domains = ["api.example.com"]
//! mode    = "extend"
//!
//! [csp.resourceDomains]
//! domains = ["cdn.example.com"]
//! mode    = "extend"
//!
//! [csp.frameDomains]
//! domains = []
//! mode    = "replace"
//!
//! [[csp.widget]]
//! match              = "ui://widget/payment*"
//! connectDomains     = ["api.stripe.com"]
//! connectDomainsMode = "extend"
//! ```

use serde::Deserialize;

/// Merge mode for a single CSP directive.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Combine the directive's declared domains with upstream.
    #[default]
    Extend,
    /// Ignore upstream for this directive; use only declared domains.
    Replace,
}

impl std::fmt::Display for Mode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Mode::Extend => write!(f, "extend"),
            Mode::Replace => write!(f, "replace"),
        }
    }
}

/// Which of the three CSP directive arrays a policy targets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Directive {
    Connect,
    Resource,
    Frame,
}

/// A domain list paired with a merge mode.
///
/// Empty `domains` combined with `Mode::Extend` is a no-op. Empty `domains`
/// combined with `Mode::Replace` explicitly clears the accumulated list.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct DirectivePolicy {
    pub domains: Vec<String>,
    pub mode: Mode,
}

impl DirectivePolicy {
    /// Build a policy that clears the accumulated list — the default for
    /// `frameDomains`, which fails closed unless the operator opts in.
    pub fn strict() -> Self {
        Self {
            domains: Vec::new(),
            mode: Mode::Replace,
        }
    }
}

/// Per-widget override matched by glob on resource URI.
///
/// Directives are addressed as two paired fields: a domains list and a mode.
/// Omitting both pairs for a directive leaves that directive untouched by the
/// widget. Setting `mode = "replace"` with an empty domains list clears the
/// accumulated domains for that directive.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct WidgetScoped {
    /// Glob pattern matched against a resource URI. `*` matches any sequence,
    /// `?` matches one character. Literal everywhere else.
    #[serde(rename = "match")]
    pub match_pattern: String,

    #[serde(rename = "connectDomains")]
    pub connect_domains: Vec<String>,
    #[serde(rename = "connectDomainsMode")]
    pub connect_domains_mode: Mode,

    #[serde(rename = "resourceDomains")]
    pub resource_domains: Vec<String>,
    #[serde(rename = "resourceDomainsMode")]
    pub resource_domains_mode: Mode,

    #[serde(rename = "frameDomains")]
    pub frame_domains: Vec<String>,
    #[serde(rename = "frameDomainsMode")]
    pub frame_domains_mode: Mode,
}

impl WidgetScoped {
    /// Fetch the (domains, mode) pair for one directive.
    fn for_directive(&self, d: Directive) -> (&[String], Mode) {
        match d {
            Directive::Connect => (&self.connect_domains, self.connect_domains_mode),
            Directive::Resource => (&self.resource_domains, self.resource_domains_mode),
            Directive::Frame => (&self.frame_domains, self.frame_domains_mode),
        }
    }
}

/// Complete CSP configuration: three global directives plus widget overrides.
#[derive(Clone, Debug)]
pub struct CspConfig {
    pub connect_domains: DirectivePolicy,
    pub resource_domains: DirectivePolicy,
    pub frame_domains: DirectivePolicy,
    pub widgets: Vec<WidgetScoped>,
}

impl Default for CspConfig {
    /// Defaults to permissive extend for connect and resource, and strict
    /// replace for frame. Frames default strict because nested iframes are
    /// rare in MCP widgets and the blast radius of an accidental allow is high.
    fn default() -> Self {
        Self {
            connect_domains: DirectivePolicy::default(),
            resource_domains: DirectivePolicy::default(),
            frame_domains: DirectivePolicy::strict(),
            widgets: Vec::new(),
        }
    }
}

impl CspConfig {
    fn policy(&self, d: Directive) -> &DirectivePolicy {
        match d {
            Directive::Connect => &self.connect_domains,
            Directive::Resource => &self.resource_domains,
            Directive::Frame => &self.frame_domains,
        }
    }
}

/// Compute the effective domain list for one directive.
///
/// - `upstream_domains` are the values the MCP server declared for this
///   directive; they pass through only when the global mode is `Extend`.
/// - `resource_uri` selects which `[[csp.widget]]` overrides apply.
/// - `upstream_host` is the bare host (no scheme) used to strip upstream
///   self-references that would leak localhost into the proxied CSP.
/// - `proxy_url` is prepended for `connect` and `resource` so widgets can
///   reach the proxy for API calls and asset loads. It is NOT prepended for
///   `frame` — widgets don't iframe the proxy back into themselves, and
///   including it there makes every widget look like an iframe-embedder.
pub fn effective_domains(
    cfg: &CspConfig,
    directive: Directive,
    resource_uri: Option<&str>,
    upstream_domains: &[String],
    upstream_host: &str,
    proxy_url: &str,
) -> Vec<String> {
    let global = cfg.policy(directive);

    // 1. Seed from upstream unless the global mode replaces it.
    let mut base: Vec<String> = if global.mode == Mode::Replace {
        Vec::new()
    } else {
        upstream_domains
            .iter()
            .filter(|d| !is_self_reference(d, upstream_host))
            .cloned()
            .collect()
    };

    // 2. Always add the globally declared domains.
    for d in &global.domains {
        push_unique(&mut base, d);
    }

    // 3. Walk widget overrides in config order. A widget with empty domains
    //    and extend mode is a no-op; we skip it to keep the diff clean.
    if let Some(uri) = resource_uri {
        for w in &cfg.widgets {
            if !glob_match(&w.match_pattern, uri) {
                continue;
            }
            let (domains, mode) = w.for_directive(directive);
            if domains.is_empty() && mode == Mode::Extend {
                continue;
            }
            if mode == Mode::Replace {
                base = domains.to_vec();
            } else {
                for d in domains {
                    push_unique(&mut base, d);
                }
            }
        }
    }

    // 4. Prepend the proxy URL for directives that need to reach the proxy
    //    itself (API calls, asset loads). Frame is intentionally skipped —
    //    see module docs. Dedupe preserving first-seen order.
    let mut out = if directive == Directive::Frame {
        Vec::new()
    } else {
        vec![proxy_url.to_string()]
    };
    for d in base {
        push_unique(&mut out, &d);
    }
    out
}

fn push_unique(list: &mut Vec<String>, value: &str) {
    if !list.iter().any(|s| s == value) {
        list.push(value.to_string());
    }
}

/// Returns true if `domain` points at the upstream MCP server itself or at
/// a local loopback address. These values only make sense in dev; the proxy
/// replaces them with its own URL so the widget reaches the proxy instead.
fn is_self_reference(domain: &str, upstream_host: &str) -> bool {
    if domain.contains("localhost") || domain.contains("127.0.0.1") {
        return true;
    }
    !upstream_host.is_empty() && domain.contains(upstream_host)
}

/// Minimal glob matcher over bytes. Supports `*` (any sequence) and `?`
/// (single character). Everything else matches literally.
pub fn glob_match(pattern: &str, input: &str) -> bool {
    glob_rec(pattern.as_bytes(), input.as_bytes())
}

fn glob_rec(p: &[u8], t: &[u8]) -> bool {
    if p.is_empty() {
        return t.is_empty();
    }
    if p[0] == b'*' {
        // `*` matches zero or more characters; try consuming none first, then
        // consume one from the input and retry. Patterns are short (tens of
        // bytes) so the recursion depth stays bounded.
        if glob_rec(&p[1..], t) {
            return true;
        }
        if !t.is_empty() {
            return glob_rec(p, &t[1..]);
        }
        return false;
    }
    if !t.is_empty() && (p[0] == b'?' || p[0] == t[0]) {
        return glob_rec(&p[1..], &t[1..]);
    }
    false
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;

    // ── helpers ────────────────────────────────────────────────────────────

    fn policy(domains: &[&str], mode: Mode) -> DirectivePolicy {
        DirectivePolicy {
            domains: domains.iter().map(|s| s.to_string()).collect(),
            mode,
        }
    }

    fn widget(pattern: &str, connect: &[&str], mode: Mode) -> WidgetScoped {
        WidgetScoped {
            match_pattern: pattern.to_string(),
            connect_domains: connect.iter().map(|s| s.to_string()).collect(),
            connect_domains_mode: mode,
            ..Default::default()
        }
    }

    fn domains(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    // ── Mode serde ─────────────────────────────────────────────────────────

    #[test]
    fn mode__deserialises_extend() {
        let m: Mode = serde_json::from_str("\"extend\"").unwrap();
        assert_eq!(m, Mode::Extend);
    }

    #[test]
    fn mode__deserialises_replace() {
        let m: Mode = serde_json::from_str("\"replace\"").unwrap();
        assert_eq!(m, Mode::Replace);
    }

    #[test]
    fn mode__default_is_extend() {
        assert_eq!(Mode::default(), Mode::Extend);
    }

    // ── CspConfig defaults ─────────────────────────────────────────────────

    #[test]
    fn csp_config__default_strict_frames() {
        let c = CspConfig::default();
        assert_eq!(c.connect_domains.mode, Mode::Extend);
        assert_eq!(c.resource_domains.mode, Mode::Extend);
        assert_eq!(c.frame_domains.mode, Mode::Replace);
    }

    // ── effective_domains: global extend ───────────────────────────────────

    #[test]
    fn effective__extend_keeps_external_drops_upstream_host() {
        let cfg = CspConfig::default();
        let upstream = domains(&["https://api.external.com", "http://localhost:9000"]);

        let out = effective_domains(
            &cfg,
            Directive::Connect,
            None,
            &upstream,
            "localhost:9000",
            "https://proxy.example.com",
        );

        assert_eq!(
            out,
            domains(&["https://proxy.example.com", "https://api.external.com"])
        );
    }

    #[test]
    fn effective__extend_adds_global_domains() {
        let cfg = CspConfig {
            connect_domains: policy(&["https://api.mine.com"], Mode::Extend),
            ..CspConfig::default()
        };

        let out = effective_domains(
            &cfg,
            Directive::Connect,
            None,
            &domains(&["https://api.external.com"]),
            "upstream.internal",
            "https://proxy.example.com",
        );

        assert_eq!(
            out,
            domains(&[
                "https://proxy.example.com",
                "https://api.external.com",
                "https://api.mine.com",
            ])
        );
    }

    // ── effective_domains: global replace ──────────────────────────────────

    #[test]
    fn effective__replace_ignores_upstream() {
        let cfg = CspConfig {
            connect_domains: policy(&["https://api.mine.com"], Mode::Replace),
            ..CspConfig::default()
        };

        let out = effective_domains(
            &cfg,
            Directive::Connect,
            None,
            &domains(&["https://api.external.com"]),
            "upstream.internal",
            "https://proxy.example.com",
        );

        assert_eq!(
            out,
            domains(&["https://proxy.example.com", "https://api.mine.com"])
        );
    }

    #[test]
    fn effective__replace_with_empty_global_leaves_only_proxy() {
        let cfg = CspConfig {
            connect_domains: policy(&[], Mode::Replace),
            ..CspConfig::default()
        };

        let out = effective_domains(
            &cfg,
            Directive::Connect,
            None,
            &domains(&["https://api.external.com"]),
            "upstream.internal",
            "https://proxy.example.com",
        );

        assert_eq!(out, domains(&["https://proxy.example.com"]));
    }

    // ── effective_domains: frame does not get the proxy URL ───────────────

    #[test]
    fn effective__frame_directive_default_is_empty_not_proxy() {
        // Frame defaults to strict replace with empty domains. Unlike connect
        // and resource, we must NOT prepend the proxy URL — widgets don't
        // iframe the proxy back into themselves, and including it flags the
        // widget as an iframe-embedder to hosts like ChatGPT.
        let cfg = CspConfig::default();
        let out = effective_domains(
            &cfg,
            Directive::Frame,
            None,
            &domains(&["https://embed.external.com"]),
            "upstream.internal",
            "https://proxy.example.com",
        );

        assert!(out.is_empty(), "expected empty, got {out:?}");
    }

    #[test]
    fn effective__frame_directive_with_declared_domains_omits_proxy() {
        // When the operator declares frame domains, they pass through verbatim
        // — still no proxy URL prefix.
        let cfg = CspConfig {
            frame_domains: policy(&["https://embed.partner.com"], Mode::Extend),
            ..CspConfig::default()
        };
        let out = effective_domains(
            &cfg,
            Directive::Frame,
            None,
            &[],
            "upstream.internal",
            "https://proxy.example.com",
        );

        assert_eq!(out, domains(&["https://embed.partner.com"]));
    }

    #[test]
    fn effective__connect_and_resource_still_get_proxy_prepend() {
        // Regression guard: the frame-skip must not affect connect/resource.
        let cfg = CspConfig::default();
        for directive in [Directive::Connect, Directive::Resource] {
            let out = effective_domains(
                &cfg,
                directive,
                None,
                &[],
                "upstream.internal",
                "https://proxy.example.com",
            );
            assert_eq!(
                out,
                domains(&["https://proxy.example.com"]),
                "directive {directive:?}"
            );
        }
    }

    // ── effective_domains: widget extend ───────────────────────────────────

    #[test]
    fn effective__widget_extend_adds_on_top_of_global() {
        let cfg = CspConfig {
            connect_domains: policy(&["https://api.mine.com"], Mode::Extend),
            widgets: vec![widget(
                "ui://widget/payment*",
                &["https://api.stripe.com"],
                Mode::Extend,
            )],
            ..CspConfig::default()
        };

        let out = effective_domains(
            &cfg,
            Directive::Connect,
            Some("ui://widget/payment-form"),
            &[],
            "upstream.internal",
            "https://proxy.example.com",
        );

        assert_eq!(
            out,
            domains(&[
                "https://proxy.example.com",
                "https://api.mine.com",
                "https://api.stripe.com",
            ])
        );
    }

    #[test]
    fn effective__widget_with_no_matching_uri_is_ignored() {
        let cfg = CspConfig {
            widgets: vec![widget(
                "ui://widget/payment*",
                &["https://api.stripe.com"],
                Mode::Extend,
            )],
            ..CspConfig::default()
        };

        let out = effective_domains(
            &cfg,
            Directive::Connect,
            Some("ui://widget/search"),
            &[],
            "upstream.internal",
            "https://proxy.example.com",
        );

        assert_eq!(out, domains(&["https://proxy.example.com"]));
    }

    #[test]
    fn effective__widget_without_uri_context_falls_back_to_global() {
        let cfg = CspConfig {
            connect_domains: policy(&["https://api.mine.com"], Mode::Extend),
            widgets: vec![widget("*", &["https://should.not.apply"], Mode::Extend)],
            ..CspConfig::default()
        };

        let out = effective_domains(
            &cfg,
            Directive::Connect,
            None,
            &[],
            "upstream.internal",
            "https://proxy.example.com",
        );

        assert_eq!(
            out,
            domains(&["https://proxy.example.com", "https://api.mine.com"])
        );
    }

    // ── effective_domains: widget replace ──────────────────────────────────

    #[test]
    fn effective__widget_replace_wipes_everything_before_it() {
        let cfg = CspConfig {
            connect_domains: policy(&["https://api.mine.com"], Mode::Extend),
            widgets: vec![widget(
                "ui://widget/payment*",
                &["https://api.stripe.com"],
                Mode::Replace,
            )],
            ..CspConfig::default()
        };

        let out = effective_domains(
            &cfg,
            Directive::Connect,
            Some("ui://widget/payment-form"),
            &domains(&["https://api.external.com"]),
            "upstream.internal",
            "https://proxy.example.com",
        );

        assert_eq!(
            out,
            domains(&["https://proxy.example.com", "https://api.stripe.com"])
        );
    }

    #[test]
    fn effective__widget_replace_with_empty_domains_clears_list() {
        let cfg = CspConfig {
            connect_domains: policy(&["https://api.mine.com"], Mode::Extend),
            widgets: vec![widget("ui://widget/*", &[], Mode::Replace)],
            ..CspConfig::default()
        };

        let out = effective_domains(
            &cfg,
            Directive::Connect,
            Some("ui://widget/anything"),
            &domains(&["https://api.external.com"]),
            "upstream.internal",
            "https://proxy.example.com",
        );

        assert_eq!(out, domains(&["https://proxy.example.com"]));
    }

    #[test]
    fn effective__widget_extend_with_empty_domains_is_noop() {
        // An empty + extend widget entry must not change anything, even when
        // it matches the URI. Operators use this shape when they set modes
        // for some directives but not others on the same widget block.
        let cfg = CspConfig {
            connect_domains: policy(&["https://api.mine.com"], Mode::Extend),
            widgets: vec![widget("ui://widget/*", &[], Mode::Extend)],
            ..CspConfig::default()
        };

        let out = effective_domains(
            &cfg,
            Directive::Connect,
            Some("ui://widget/anything"),
            &[],
            "upstream.internal",
            "https://proxy.example.com",
        );

        assert_eq!(
            out,
            domains(&["https://proxy.example.com", "https://api.mine.com"])
        );
    }

    // ── effective_domains: widget ordering ────────────────────────────────

    #[test]
    fn effective__multiple_matching_widgets_apply_in_config_order() {
        let cfg = CspConfig {
            widgets: vec![
                widget("ui://widget/*", &["https://a.com"], Mode::Extend),
                widget("ui://widget/*", &["https://b.com"], Mode::Replace),
                widget("ui://widget/*", &["https://c.com"], Mode::Extend),
            ],
            ..CspConfig::default()
        };

        let out = effective_domains(
            &cfg,
            Directive::Connect,
            Some("ui://widget/anything"),
            &[],
            "upstream.internal",
            "https://proxy.example.com",
        );

        // First widget extends with a.com, second wipes and sets b.com,
        // third extends with c.com. Proxy URL is always prepended last.
        assert_eq!(
            out,
            domains(&[
                "https://proxy.example.com",
                "https://b.com",
                "https://c.com"
            ])
        );
    }

    // ── effective_domains: dedupe ──────────────────────────────────────────

    #[test]
    fn effective__dedupes_across_sources() {
        let cfg = CspConfig {
            connect_domains: policy(&["https://shared.com"], Mode::Extend),
            widgets: vec![widget(
                "ui://widget/*",
                &["https://shared.com"],
                Mode::Extend,
            )],
            ..CspConfig::default()
        };

        let out = effective_domains(
            &cfg,
            Directive::Connect,
            Some("ui://widget/x"),
            &domains(&["https://shared.com"]),
            "upstream.internal",
            "https://proxy.example.com",
        );

        assert_eq!(
            out,
            domains(&["https://proxy.example.com", "https://shared.com"])
        );
    }

    #[test]
    fn effective__dedupes_proxy_url_already_in_upstream() {
        let cfg = CspConfig::default();
        let out = effective_domains(
            &cfg,
            Directive::Connect,
            None,
            &domains(&["https://proxy.example.com", "https://api.external.com"]),
            "upstream.internal",
            "https://proxy.example.com",
        );
        let count = out
            .iter()
            .filter(|d| *d == "https://proxy.example.com")
            .count();
        assert_eq!(count, 1);
    }

    // ── self-reference stripping ───────────────────────────────────────────

    #[test]
    fn effective__strips_localhost() {
        let cfg = CspConfig::default();
        let out = effective_domains(
            &cfg,
            Directive::Connect,
            None,
            &domains(&["http://localhost:9000", "http://127.0.0.1:9000"]),
            "upstream.internal",
            "https://proxy.example.com",
        );
        assert_eq!(out, domains(&["https://proxy.example.com"]));
    }

    #[test]
    fn effective__strips_upstream_host() {
        let cfg = CspConfig::default();
        let out = effective_domains(
            &cfg,
            Directive::Connect,
            None,
            &domains(&["https://upstream.internal", "https://api.external.com"]),
            "upstream.internal",
            "https://proxy.example.com",
        );
        assert_eq!(
            out,
            domains(&["https://proxy.example.com", "https://api.external.com"])
        );
    }

    #[test]
    fn effective__empty_upstream_host_disables_self_stripping() {
        // When the upstream host is unknown (empty), only localhost heuristics
        // remain. External domains pass through.
        let cfg = CspConfig::default();
        let out = effective_domains(
            &cfg,
            Directive::Connect,
            None,
            &domains(&["https://api.external.com"]),
            "",
            "https://proxy.example.com",
        );
        assert_eq!(
            out,
            domains(&["https://proxy.example.com", "https://api.external.com"])
        );
    }

    // ── glob_match ─────────────────────────────────────────────────────────

    #[test]
    fn glob__literal_match() {
        assert!(glob_match("ui://widget/payment", "ui://widget/payment"));
    }

    #[test]
    fn glob__literal_mismatch() {
        assert!(!glob_match("ui://widget/payment", "ui://widget/search"));
    }

    #[test]
    fn glob__star_matches_suffix() {
        assert!(glob_match(
            "ui://widget/payment*",
            "ui://widget/payment-form"
        ));
        assert!(glob_match("ui://widget/payment*", "ui://widget/payment"));
    }

    #[test]
    fn glob__star_matches_any_sequence() {
        assert!(glob_match("ui://*/payment", "ui://widget/payment"));
        assert!(glob_match("ui://*/payment", "ui://nested/a/b/payment"));
    }

    #[test]
    fn glob__double_star_segment() {
        assert!(glob_match("ui://widget/*", "ui://widget/anything"));
    }

    #[test]
    fn glob__question_matches_single_char() {
        assert!(glob_match("ui://widget/a?c", "ui://widget/abc"));
        assert!(!glob_match("ui://widget/a?c", "ui://widget/ac"));
    }

    #[test]
    fn glob__empty_pattern_matches_empty_string_only() {
        assert!(glob_match("", ""));
        assert!(!glob_match("", "anything"));
    }

    #[test]
    fn glob__star_only_matches_anything() {
        assert!(glob_match("*", ""));
        assert!(glob_match("*", "anything"));
    }

    // ── Mode Display ───────────────────────────────────────────────────────

    #[test]
    fn mode__display() {
        assert_eq!(Mode::Extend.to_string(), "extend");
        assert_eq!(Mode::Replace.to_string(), "replace");
    }
}
