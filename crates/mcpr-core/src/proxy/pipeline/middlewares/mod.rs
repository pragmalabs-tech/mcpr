//! Middleware implementations — one struct per file, each ported from
//! a `pipeline/steps/*.rs` free function.
//!
//! These live alongside the old `steps/` functions during Phase 3. Phase
//! 5 flips the live pipeline over to this module and deletes `steps/`.
//! Until then, no production code path references anything here.

pub mod client_info_inject;
pub mod csp_rewrite;
pub mod envelope_seal;
pub mod health_track;
pub mod schema_ingest;
pub mod schema_stale;
pub mod session_delete;
pub mod session_record;
pub mod session_touch;
pub mod url_map;

pub(crate) mod shared;

#[cfg(test)]
pub(crate) mod test_support;

pub use client_info_inject::ClientInfoInjectMiddleware;
pub use csp_rewrite::CspRewriteMiddleware;
pub use envelope_seal::EnvelopeSealMiddleware;
pub use health_track::HealthTrackMiddleware;
pub use schema_ingest::SchemaIngestMiddleware;
pub use schema_stale::SchemaStaleMiddleware;
pub use session_delete::SessionDeleteMiddleware;
pub use session_record::SessionRecordMiddleware;
pub use session_touch::SessionTouchMiddleware;
pub use url_map::UrlMapMiddleware;
