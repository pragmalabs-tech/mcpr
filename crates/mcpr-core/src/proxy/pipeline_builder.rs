//! Default middleware chain construction for the pipeline.
//!
//! One call to [`build_default_pipeline`] produces the ordered chain
//! from `PIPELINE.md` §Middleware. Order matters:
//! schema ingest runs before CSP rewrite so the schema store captures
//! the raw upstream CSP (cloud-backend contract), and envelope seal runs
//! last so content-inspecting middlewares operate on `McpBuffered`
//! before it becomes a sealed `Raw`.

use std::sync::Arc;

use arc_swap::ArcSwap;

use super::RewriteConfig;
use super::pipeline::driver::Pipeline;
use super::pipeline::middleware::{RequestMiddleware, ResponseMiddleware};
use super::pipeline::middlewares::{
    ClientInfoInjectMiddleware, CspRewriteMiddleware, EnvelopeSealMiddleware,
    HealthTrackMiddleware, SchemaIngestMiddleware, SchemaStaleMiddleware, SessionDeleteMiddleware,
    SessionRecordMiddleware, SessionTouchMiddleware, UrlMapMiddleware,
};
use super::router::ProxyRouter;
use super::transport::ProxyTransport;

/// Concrete `Pipeline` instantiation used in production. Middleware
/// traits are object-safe (`Box<dyn …>`), but the router and transport
/// are generic parameters that want concrete types at construction.
pub type ProxyPipeline = Pipeline<ProxyRouter, ProxyTransport>;

/// Build the baseline pipeline. `rewrite_config` is shared with the
/// middlewares that read it (`CspRewrite`, `UrlMap`) — swapping the
/// inner `Arc` via `.store()` hot-reloads rules without restart.
pub fn build_default_pipeline(rewrite_config: Arc<ArcSwap<RewriteConfig>>) -> ProxyPipeline {
    let request_chain: Vec<Box<dyn RequestMiddleware>> = vec![
        Box::new(SessionDeleteMiddleware),
        Box::new(SessionTouchMiddleware),
        Box::new(ClientInfoInjectMiddleware),
    ];
    let response_chain: Vec<Box<dyn ResponseMiddleware>> = vec![
        // `SchemaIngest` reads the raw upstream result BEFORE `CspRewrite`
        // mutates it — the schema store must capture the untouched CSP.
        Box::new(SchemaIngestMiddleware),
        Box::new(SchemaStaleMiddleware),
        Box::new(CspRewriteMiddleware::new(rewrite_config.clone())),
        Box::new(SessionRecordMiddleware),
        Box::new(HealthTrackMiddleware),
        // `UrlMap` only touches `OauthJson` / `Raw-with-JSON-content-type`
        // that came from the passthrough dispatch — ordering it before
        // `EnvelopeSeal` keeps it from also re-rewriting the sealed
        // `McpBuffered → Raw` bytes.
        Box::new(UrlMapMiddleware::new(rewrite_config)),
        Box::new(EnvelopeSealMiddleware),
    ];

    for mw in &request_chain {
        tracing::info!(chain = "request", name = mw.name(), "middleware registered");
    }
    for mw in &response_chain {
        tracing::info!(
            chain = "response",
            name = mw.name(),
            "middleware registered"
        );
    }

    Pipeline::new(request_chain, response_chain, ProxyRouter, ProxyTransport)
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;

    use crate::proxy::pipeline::middlewares::test_support::test_proxy_state;

    #[tokio::test]
    async fn build_default_pipeline__registers_expected_chain_names_in_order() {
        let proxy = test_proxy_state();
        let pipeline = build_default_pipeline(proxy.rewrite_config.clone());

        assert_eq!(
            pipeline.request_chain_names(),
            vec!["session_delete", "session_touch", "client_info_inject"],
        );
        assert_eq!(
            pipeline.response_chain_names(),
            vec![
                "schema_ingest",
                "schema_stale",
                "csp_rewrite",
                "session_record",
                "health_track",
                "url_map",
                "envelope_seal",
            ],
        );
    }
}
