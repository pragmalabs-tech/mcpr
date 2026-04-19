//! Header-only request-phase steps. Cheap, synchronous-ish (awaits
//! only on async store ops), no body access. Runs before request
//! classification so session touch / DELETE cleanup apply uniformly.
//!
//! Replaces `middleware::SessionTouchMiddleware` and
//! `middleware::DeleteSessionEndMiddleware`.

use axum::response::Response;

use crate::proxy::ProxyState;
use crate::proxy::pipeline::context::RequestContext;
use crate::proxy::pipeline::steps::session;

/// Run all header-phase steps. Returns `Some(response)` only if a step
/// needs to short-circuit; currently always returns `None`, but the
/// signature leaves room for e.g. auth-rejection in future.
pub async fn header_phase(state: &ProxyState, ctx: &RequestContext) -> Option<Response> {
    session::touch(state, ctx).await;
    session::maybe_handle_delete(state, ctx).await
}
