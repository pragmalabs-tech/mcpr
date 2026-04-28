//! HTTP request/response shapes for non-MCP traffic flowing through the proxy.
//!
//! Both directions buffer the body into `Bytes`, so headers, status, and
//! payload are all owned and inspectable — what the proxy needs to route,
//! rewrite, and forward.

use axum::body::Bytes;

pub type HttpRequest = axum::http::Request<Bytes>;
pub type Result = axum::http::Response<Bytes>;
