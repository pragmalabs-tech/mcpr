/// Log Stage will focus to track request and response events;
use async_trait::async_trait;

use crate::{
    event::ProxyEvent,
    protocol::{Request, Response},
    proxy2::{
        stage::types::{RequestStage, ResponseStage},
        state::ProxyState,
    },
};

pub struct RequestLogStage;

#[async_trait]
impl RequestStage for RequestLogStage {
    async fn process(&self, request: Request, state: ProxyState) -> anyhow::Result<Request> {
        state
            .event_bus
            .emit(ProxyEvent::Request(Box::new(request.clone())));

        Ok(request)
    }
}

pub struct ResponseLogStage;

#[async_trait]
impl ResponseStage for ResponseLogStage {
    async fn process(&self, res: Response, state: ProxyState) -> anyhow::Result<Response> {
        state
            .event_bus
            .emit(ProxyEvent::Response(Box::new(res.clone())));

        Ok(res)
    }
}
