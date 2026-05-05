use std::sync::Arc;

use crate::{auth::AuthProvider, event::EventBus, protocol::session::SessionStore};

// This is safe to just use Arc for InnerProxyState
// because we will just use ref and never touch to mutate any data.
pub type ProxyState = Arc<InnerProxyState>;

pub struct InnerProxyState {
    pub event_bus: EventBus,
    pub sessions: SessionStore,
    /// OAuth 2.1 provider. `None` when the proxy is running without
    /// `[auth]` configured; in that case the discovery route is not
    /// mounted and the proxy stays auth-transparent.
    pub auth_provider: Option<Arc<dyn AuthProvider>>,
}

impl InnerProxyState {
    pub fn new(
        event_bus: EventBus,
        sessions: SessionStore,
        auth_provider: Option<Arc<dyn AuthProvider>>,
    ) -> Self {
        Self {
            event_bus,
            sessions,
            auth_provider,
        }
    }

    #[cfg(test)]
    pub fn for_tests() -> ProxyState {
        Arc::new(Self::new(EventBus::for_tests(), SessionStore::new(), None))
    }
}
