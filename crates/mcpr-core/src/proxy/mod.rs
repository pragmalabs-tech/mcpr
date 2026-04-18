//! # mcpr-proxy
//!
//! Proxy engine for mcpr: request routing, upstream forwarding, SSE streaming,
//! and widget CSP rewriting.
//!
//! This crate sits between MCP clients and upstream MCP servers. It classifies
//! requests, forwards them over HTTP, relays SSE streams, and rewrites widget
//! CSP metadata on the way back.
//!
//! ## Responsibilities
//!
//! - **Request routing** (`router`): Classify incoming HTTP requests into typed
//!   variants — MCP JSON-RPC POST, MCP SSE GET, widget HTML, widget assets,
//!   OAuth callbacks, or passthrough.
//!
//! - **Upstream forwarding** (`forwarding`): HTTP client with connection pooling,
//!   semaphore-based concurrency limiting, configurable timeouts, and header
//!   forwarding (auth, content-type, MCP session ID).
//!
//! - **SSE handling** (`sse`): Extract JSON from SSE-wrapped responses, re-wrap
//!   JSON as SSE, and split upstream URLs into (base, path) components.
//!
//! - **Widget CSP** (`csp`, `rewrite`): Declarative CSP config with per-directive
//!   modes and widget-scoped overrides. `csp::effective_domains` computes the
//!   final domain list for one directive; `rewrite::rewrite_response` applies
//!   that to every CSP array in a JSON-RPC response.
//!
//! - **Proxy state** (`state`): Shared runtime state tracking MCP upstream
//!   health, tunnel status, widget discovery, cloud sync, and request counters.
//!
//! ## Module layout
//!
//! ```text
//! proxy/
//! ├── router.rs       ClassifiedRequest, classify()
//! ├── forwarding.rs   UpstreamClient, forward_request()
//! ├── sse.rs          SSE extract/wrap helpers
//! ├── csp.rs          CspConfig, DirectivePolicy, WidgetScoped, effective_domains
//! ├── rewrite.rs      RewriteConfig, rewrite_response()
//! └── health.rs       ProxyHealth, ConnectionStatus, SharedProxyHealth
//! ```

pub mod csp;
pub mod forwarding;
pub mod health;
pub mod rewrite;
pub mod router;
pub mod sse;

pub use csp::{
    CspConfig, Directive, DirectivePolicy, Mode, WidgetScoped, effective_domains, glob_match,
};
pub use health::{ConnectionStatus, ProxyHealth, SharedProxyHealth, lock_health, new_shared_health};
pub use rewrite::{RewriteConfig, rewrite_response};
