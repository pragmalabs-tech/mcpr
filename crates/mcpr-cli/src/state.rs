//! [`AppState`] — host-level axum state container.
//!
//! Today wraps a single [`ProxyState`]; future phases grow this to a map
//! keyed by proxy name once multi-proxy-per-process lands.

use std::sync::Arc;

use axum::extract::FromRef;
use mcpr_core::proxy::ProxyState;

/// Host-level container owning the proxy instance(s) this process runs.
///
/// Cloneable because axum `with_state` requires `Clone`. Cloning is cheap —
/// only `Arc` bumps.
#[derive(Clone)]
pub struct AppState {
    pub proxy: Arc<ProxyState>,
}

/// Axum extractor glue: handlers that only need the proxy can write
/// `State<Arc<ProxyState>>` and axum will pull it out of `AppState`.
impl FromRef<AppState> for Arc<ProxyState> {
    fn from_ref(app: &AppState) -> Arc<ProxyState> {
        app.proxy.clone()
    }
}
