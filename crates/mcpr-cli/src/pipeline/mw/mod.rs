//! Response middleware — composable transforms over
//! [`ResponseContext`](super::context::ResponseContext).
//!
//! Each mw is a standalone type implementing [`ResponseMw`]. Handlers call
//! them in a fixed order today; Phase 6 introduces a declarative registration
//! pipeline.

use async_trait::async_trait;

use super::context::{RequestContext, ResponseContext};
use crate::state::ProxyState;

pub mod rewrite_url;
pub mod schema;
pub mod sse;
pub mod upstream_url_map;

pub use rewrite_url::UrlRewriteMw;
pub use schema::{SchemaIngestMw, StaleMarkMw};
pub use sse::{SseUnwrapMw, SseWrapMw};
pub use upstream_url_map::UpstreamUrlMapMw;

/// A single step that reads and mutates the response. Implementations must be
/// side-effect-free with respect to external state beyond the bus/session
/// store/schema manager (which are part of [`ProxyState`]).
#[async_trait]
pub trait ResponseMw: Send + Sync {
    async fn on_response(
        &self,
        state: &ProxyState,
        req: &RequestContext,
        resp: &mut ResponseContext,
    );
}
