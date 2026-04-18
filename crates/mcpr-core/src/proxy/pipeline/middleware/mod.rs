//! Request and response middleware — composable transforms over
//! [`RequestContext`](super::context::RequestContext) and
//! [`ResponseContext`](super::context::ResponseContext).
//!
//! Each middleware is a standalone type implementing [`RequestMiddleware`]
//! or [`ResponseMiddleware`]. The pipeline runner calls them in a fixed
//! order per route.

use async_trait::async_trait;
use axum::response::Response;

use super::context::{RequestContext, ResponseContext};
use crate::proxy::proxy_state::ProxyState;

pub mod health;
pub mod rewrite_url;
pub mod schema;
pub mod session;
pub mod sse;
pub mod upstream_url_map;
pub mod widget_overlay;

pub use health::McpHealthMiddleware;
pub use rewrite_url::UrlRewriteMiddleware;
pub use schema::{SchemaIngestMiddleware, StaleMarkMiddleware};
pub use session::{DeleteSessionEndMiddleware, SessionStartMiddleware, SessionTouchMiddleware};
pub use sse::{SseUnwrapMiddleware, SseWrapMiddleware};
pub use upstream_url_map::UpstreamUrlMapMiddleware;
pub use widget_overlay::WidgetOverlayMiddleware;

/// A pre-forward step that reads and mutates the request. Returning
/// `Some(response)` short-circuits the pipeline (handler + response middleware are
/// skipped; the response goes straight to the emit stage).
#[async_trait]
pub trait RequestMiddleware: Send + Sync {
    async fn on_request(&self, state: &ProxyState, ctx: &mut RequestContext) -> Option<Response>;
}

/// A post-forward step that reads and mutates the response. Implementations
/// must be side-effect-free with respect to external state beyond the
/// bus/session store/schema manager (which are part of [`ProxyState`]).
#[async_trait]
pub trait ResponseMiddleware: Send + Sync {
    async fn on_response(
        &self,
        state: &ProxyState,
        req: &RequestContext,
        resp: &mut ResponseContext,
    );
}
