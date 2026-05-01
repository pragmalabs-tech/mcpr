/// Log Stage will focus to track request and response events;
use std::sync::Arc;

use async_trait::async_trait;

use crate::{
    event::ProxyEvent,
    protocol::{Request, Response},
    proxy2::{
        stage::types::{RequestContext, RequestStage, ResponseStage},
        state::ProxyState,
    },
};

pub struct RequestLogStage;

#[async_trait]
impl RequestStage for RequestLogStage {
    fn name(&self) -> &'static str {
        "RequestLogStage"
    }

    async fn process(
        &self,
        request: Request,
        _request_ctx: RequestContext,
        state: ProxyState,
    ) -> anyhow::Result<Request> {
        state
            .event_bus
            .emit(ProxyEvent::Request(Arc::new(request.clone())));

        Ok(request)
    }
}

pub struct ResponseLogStage;

#[async_trait]
impl ResponseStage for ResponseLogStage {
    fn name(&self) -> &'static str {
        "ResponseLogStage"
    }

    async fn process(
        &self,
        res: Response,
        _request_ctx: RequestContext,
        state: ProxyState,
    ) -> anyhow::Result<Response> {
        state
            .event_bus
            .emit(ProxyEvent::Response(Arc::new(res.clone())));

        Ok(res)
    }
}
