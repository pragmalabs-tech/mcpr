//! Widget CSP rewrite step.
//!
//! Replaces `middleware::UrlRewriteMiddleware` with two explicit
//! functions. Callers use [`has_markers`] as a cheap pre-scan — if the
//! body contains no CSP-shaped keys, we skip the JSON parse entirely.
//! [`rewrite_in_place`] applies the existing `rewrite_response` logic
//! and returns `true` iff any mutation actually happened.

use serde_json::Value;

use arc_swap::ArcSwap;

use crate::proxy::{RewriteConfig, rewrite_response};

/// CSP-shaped keys that `rewrite_response` / `inject_proxy_into_all_csp`
/// can mutate. If none of these appear as a substring anywhere in the
/// body bytes, there's nothing to rewrite — skip the JSON parse and the
/// full tree walk.
const MARKERS: &[&[u8]] = &[
    b"connect_domains",
    b"resource_domains",
    b"frame_domains",
    b"connectDomains",
    b"resourceDomains",
    b"frameDomains",
    b"openai/widgetCSP",
    b"ui.csp",
    b"openai/widgetDomain",
];

/// `true` if the body *might* contain a CSP marker. False positives are
/// fine (we fall through to the full rewrite which is still correct);
/// false negatives would silently drop a rewrite, so the list must
/// cover every key `rewrite_response` looks at.
#[must_use]
pub fn has_markers(body: &[u8]) -> bool {
    MARKERS.iter().any(|m| contains_slice(body, m))
}

fn contains_slice(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || haystack.len() < needle.len() {
        return needle.is_empty();
    }
    haystack.windows(needle.len()).any(|win| win == needle)
}

/// Apply `rewrite_response` to the parsed JSON in place. Returns `true`
/// iff the JSON was actually mutated (caller should reserialize).
#[must_use]
pub fn rewrite_in_place(config: &ArcSwap<RewriteConfig>, method: &str, parsed: &mut Value) -> bool {
    let cfg = config.load();
    rewrite_response(method, parsed, &cfg)
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;

    #[test]
    fn has_markers__finds_snake_case_array_key() {
        let body = br#"{"result":{"connect_domains":["http://a"]}}"#;
        assert!(has_markers(body));
    }

    #[test]
    fn has_markers__finds_camel_case_array_key() {
        let body = br#"{"result":{"connectDomains":["http://a"]}}"#;
        assert!(has_markers(body));
    }

    #[test]
    fn has_markers__finds_openai_shape() {
        let body = br#"{"meta":{"openai/widgetCSP":{}}}"#;
        assert!(has_markers(body));
    }

    #[test]
    fn has_markers__plain_tool_call_no_markers() {
        let body =
            br#"{"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"hi"}]}}"#;
        assert!(!has_markers(body));
    }

    #[test]
    fn has_markers__empty_body() {
        assert!(!has_markers(b""));
    }
}
