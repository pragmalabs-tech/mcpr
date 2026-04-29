use std::sync::Arc;

use crate::{event::EventBus, protocol::session::SessionStore};

// This is safe to just use Arc for InnerProxyState
// because we will just use ref and never touch to mutate any data.
pub type ProxyState = Arc<InnerProxyState>;

pub struct InnerProxyState {
    pub event_bus: EventBus,
    pub sessions: SessionStore,
}

impl InnerProxyState {
    pub fn new(event_bus: EventBus, sessions: SessionStore) -> Self {
        Self {
            event_bus,
            sessions,
        }
    }

    #[cfg(test)]
    pub fn for_tests() -> ProxyState {
        Arc::new(Self::new(EventBus::for_tests(), SessionStore::new()))
    }
}
