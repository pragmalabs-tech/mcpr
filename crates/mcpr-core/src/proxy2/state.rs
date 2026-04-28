use std::sync::Arc;

use crate::event::EventBus;

// This is safe to just use Arc for InnerProxyState
// because we will just use ref and never touch to mutate any data.
pub type ProxyState = Arc<InnerProxyState>;

pub struct InnerProxyState {
    pub event_bus: EventBus,
}

impl InnerProxyState {
    pub fn new(event_bus: EventBus) -> Self {
        Self { event_bus }
    }

    #[cfg(test)]
    pub fn for_tests() -> ProxyState {
        Arc::new(Self::new(EventBus::for_tests()))
    }
}
