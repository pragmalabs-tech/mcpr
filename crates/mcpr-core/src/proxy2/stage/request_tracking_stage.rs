/// Log Stage will focus to track request and response events;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;

use crate::{
    event::{
        ProxyEvent,
        types::{RequestEvent, ResponseEvent},
    },
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
        request_ctx: RequestContext,
        state: ProxyState,
    ) -> anyhow::Result<Request> {
        state
            .event_bus
            .emit(ProxyEvent::Request(Arc::new(RequestEvent {
                request: request.clone(),
                request_id: request_ctx.request_id.clone(),
                ts: Utc::now(),
            })));

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
        request_ctx: RequestContext,
        state: ProxyState,
    ) -> anyhow::Result<Response> {
        let latency_us =
            u64::try_from(request_ctx.started_at.elapsed().as_micros()).unwrap_or(u64::MAX);
        state
            .event_bus
            .emit(ProxyEvent::Response(Arc::new(ResponseEvent {
                response: res.clone(),
                request_id: request_ctx.request_id.clone(),
                latency_us,
                ts: Utc::now(),
            })));

        Ok(res)
    }
}
