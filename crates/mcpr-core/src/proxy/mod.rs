//! # mcpr-proxy
//!
//! Full MCP proxy engine: per-request pipeline (intake → request chain
//! → router → transport → response chain → emit), upstream forwarding,
//! SSE streaming, widget CSP rewriting, per-proxy health. Embed this
//! crate and wire a frontend (axum, warp, anything) around
//! [`build_default_pipeline`] + [`pipeline::driver::Pipeline::run`].
//!
//! ## Module layout
//!
//! ```text
//! proxy/
//! ├── pipeline/       Per-request pipeline (parse → route → mw → emit)
//! ├── proxy_state.rs  ProxyState — the runtime one proxy instance holds
//! ├── forwarding.rs   UpstreamClient, forward_request, read_body_capped
//! ├── sse.rs          SSE extract/wrap helpers
//! ├── csp.rs          CspConfig, DirectivePolicy, WidgetScoped
//! ├── rewrite.rs      RewriteConfig, rewrite_response (widget CSP)
//! └── health.rs       ProxyHealth, ConnectionStatus, SharedProxyHealth
//! ```

pub mod csp;
pub mod emit;
pub mod forwarding;
pub mod health;
pub mod intake;
pub mod pipeline;
pub mod pipeline_builder;
pub mod proxy_state;
pub mod rewrite;
pub mod router;
pub mod sse;
pub mod transport;

pub use csp::{
    CspConfig, Directive, DirectivePolicy, Mode, WidgetScoped, effective_domains, glob_match,
};
pub use health::{
    ConnectionStatus, ProxyHealth, SharedProxyHealth, lock_health, new_shared_health,
};
pub use intake::from_axum_parts;
pub use pipeline_builder::{ProxyPipeline, build_default_pipeline};
pub use proxy_state::ProxyState;
pub use rewrite::{RewriteConfig, rewrite_response};
pub use router::ProxyRouter;
pub use transport::ProxyTransport;
