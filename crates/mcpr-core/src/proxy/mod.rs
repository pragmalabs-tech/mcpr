//! # mcpr-proxy
//!
//! Proxy engine for mcpr: request routing, upstream forwarding, SSE streaming,
//! and widget CSP rewriting.
//!
//! This crate contains the core proxy logic that sits between clients and
//! upstream MCP servers. It handles request classification, HTTP forwarding,
//! SSE stream management, and response rewriting for widget security (CSP).
//!
//! ## Responsibilities
//!
//! - **Request routing** (`router`): Classify incoming HTTP requests into typed
//!   variants: MCP JSON-RPC POST, MCP SSE GET, widget HTML, widget assets,
//!   OAuth callbacks, or passthrough.
//!
//! - **Upstream forwarding** (`forwarding`): HTTP client with connection pooling,
//!   semaphore-based concurrency limiting, configurable timeouts, and header
//!   forwarding (auth, content-type, MCP session ID).
//!
//! - **SSE handling** (`sse`): Extract JSON from SSE-wrapped responses, re-wrap
//!   JSON as SSE, and split upstream URLs into (base, path) components.
//!
//! - **Widget CSP rewriting** (`csp`, `rewrite`): Rewrite MCP response metadata
//!   to inject proxy domains into widget CSP arrays and `widgetDomain` fields.
//!   Supports both OpenAI and Claude widget metadata formats, with `Extend` and
//!   `Override` CSP modes.
//!
//! - **Proxy state** (`state`): Shared runtime state tracking MCP upstream
//!   health, tunnel status, widget discovery, cloud sync, and request counters.
//!   Used by the admin API, health checks, and future `mcpr proxy view` TUI.
//!
//! ## Module Structure
//!
//! ```text
//! mcpr-proxy/src/
//! +-- lib.rs          # Crate root, re-exports
//! +-- router.rs       # ClassifiedRequest enum, classify() dispatcher
//! +-- forwarding.rs   # UpstreamClient, forward_request(), build_response()
//! +-- sse.rs          # SSE extract/wrap helpers, split_upstream()
//! +-- csp.rs          # CspMode enum, parse_csp_mode()
//! +-- rewrite.rs      # RewriteConfig, rewrite_response() for widget metadata
//! +-- state.rs        # ProxyState, ConnectionStatus, SharedProxyState
//! ```

pub mod csp;
pub mod forwarding;
pub mod rewrite;
pub mod router;
pub mod sse;
pub mod state;

pub use csp::{CspMode, parse_csp_mode};
pub use rewrite::{RewriteConfig, rewrite_response};
pub use state::{ConnectionStatus, ProxyState, SharedProxyState, lock_state, new_shared_state};
