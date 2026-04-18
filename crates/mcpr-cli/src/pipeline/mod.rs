//! Per-request proxy pipeline: parse → route → request mw → handler →
//! response mw → emit. Phase 2 introduces the data structures and the
//! single emit site; middleware traits land in later phases.

pub mod context;
pub mod emit;
pub mod mw;
pub mod parser;

pub use context::{RequestContext, ResponseContext};
pub use emit::{ResponseSummary, emit_request_event, normalize_platform};
pub use mw::{
    ResponseMw, SchemaIngestMw, SseUnwrapMw, SseWrapMw, StaleMarkMw, UpstreamUrlMapMw, UrlRewriteMw,
};
pub use parser::build_request_context;
