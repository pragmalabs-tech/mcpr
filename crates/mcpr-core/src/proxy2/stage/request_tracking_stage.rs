//! Request/response logging stage. Emits one `ProxyEvent::Request`
//! after the response stage completes, carrying both halves of the
//! transaction plus the per-request timer snapshot.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;

use crate::{
    event::{
        ProxyEvent,
        types::{LoggedResponse, RequestEvent},
    },
    protocol::Response,
    proxy2::{
        stage::types::{RequestContext, ResponseStage},
        state::ProxyState,
    },
};

/// Span name set by the pipeline router for the upstream call. Looked up
/// from the per-request `Timer` to populate `RequestEvent.upstream_us`.
const UPSTREAM_SPAN: &str = "Router";

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
        let spans = request_ctx.timer.to_spans_us();
        let upstream_us = spans
            .iter()
            .find(|(name, _)| name == UPSTREAM_SPAN)
            .map(|(_, us)| *us)
            .unwrap_or(0);

        let Some(req_arc) = request_ctx.request.as_ref() else {
            return Ok(res);
        };

        let mut logged: LoggedResponse = (&res).into();
        logged.slim_resources_in_place(&request_ctx.client_methods);

        state
            .event_bus
            .emit(ProxyEvent::Request(Arc::new(RequestEvent {
                request_id: request_ctx.request_id.clone(),
                request: req_arc.as_ref().into(),
                response: Some(logged),
                ts: Utc::now(),
                latency_us,
                upstream_us,
                spans,
            })));

        Ok(res)
    }
}
