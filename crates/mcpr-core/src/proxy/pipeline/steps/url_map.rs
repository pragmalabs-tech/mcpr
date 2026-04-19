//! Passthrough URL-substitution step.
//!
//! Replaces `middleware::UpstreamUrlMapMiddleware` as a function. Only
//! runs for JSON passthrough responses; gated on content-type by the
//! caller (passthrough handler). Includes a substring-presence fast
//! path so responses without the upstream URL skip allocation.

use arc_swap::ArcSwap;
use axum::body::Bytes;

use crate::proxy::{RewriteConfig, sse::split_upstream};

/// Rewrite any upstream base URL occurrences in the body to the proxy URL.
/// Caller must have already gated on content-type = JSON.
///
/// Fast path: if the body doesn't contain the upstream base as a
/// substring, return bytes unchanged — no allocation.
#[must_use]
pub fn rewrite_passthrough_urls(config: &ArcSwap<RewriteConfig>, body: Bytes) -> Bytes {
    let cfg = config.load();
    let (upstream_base, _) = split_upstream(&cfg.mcp_upstream);
    let upstream_base = upstream_base.trim_end_matches('/');
    let proxy_url = cfg.proxy_url.trim_end_matches('/');

    // Cheap substring check first — avoids the UTF-8 conversion and
    // allocation when there's nothing to rewrite.
    if !contains_slice(&body, upstream_base.as_bytes()) {
        return body;
    }

    // Fall back to the existing String::replace approach. We only reach
    // this when the substring is known to appear, so the allocation is
    // justified.
    let body_str = String::from_utf8_lossy(&body);
    Bytes::from(body_str.replace(upstream_base, proxy_url).into_bytes())
}

fn contains_slice(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || haystack.len() < needle.len() {
        return needle.is_empty();
    }
    haystack.windows(needle.len()).any(|win| win == needle)
}
