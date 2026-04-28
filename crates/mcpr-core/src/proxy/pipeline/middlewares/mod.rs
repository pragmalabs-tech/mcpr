//! Middleware implementations — one struct per file. Each implements
//! `RequestMiddleware` or `ResponseMiddleware` and matches on the
//! `Request` / `Response` variants it cares about.

pub mod client_info_inject;
pub mod csp_rewrite;
pub mod envelope_seal;
pub mod health_track;
pub mod session_delete;
pub mod session_record;
pub mod session_touch;
pub mod target_extract;
pub mod url_map;

pub(crate) mod shared;

#[cfg(test)]
pub(crate) mod test_support;

pub use client_info_inject::ClientInfoInjectMiddleware;
pub use csp_rewrite::CspRewriteMiddleware;
pub use envelope_seal::EnvelopeSealMiddleware;
pub use health_track::HealthTrackMiddleware;
pub use session_delete::SessionDeleteMiddleware;
pub use session_record::SessionRecordMiddleware;
pub use session_touch::SessionTouchMiddleware;
pub use target_extract::TargetExtractMiddleware;
pub use url_map::UrlMapMiddleware;
