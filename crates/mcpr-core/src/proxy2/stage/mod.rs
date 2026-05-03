//! Request → router → response stage chain. Request stages mutate
//! inbound traffic in order, `RouterStage` talks to upstream, response
//! stages mutate outbound traffic in order on the way back.
//!
//! TODO: vision is 1 request (has batch mcp requests) -> N responses (streaming).
//!  Today `ResponseStage` is invoked once per HTTP response and receives the
//! whole `Response` (`Mcp` / `McpBatch` / `Http`); every stage that
//! cares about individual results has to manually match the variant
//! and iterate. Refactor to per-message: change the trait to
//! `on_message(JsonRpcResult) -> JsonRpcResult` and have the pipeline
//! iterate. `Mcp` runs stages once, `McpBatch` runs them N times,
//! and a future `Response::Stream` runs them per frame as it arrives.
//! That removes the batch-handling boilerplate, makes per-result
//! encoding natural, and lets streaming drop in without touching
//! stages.

pub mod csp_rewritten_stage;
pub mod request_tracking_stage;
pub mod router_stage;
pub mod schema_tracking_stage;
pub mod session_tracking_stage;
pub mod types;

use std::sync::Arc;

use axum::http::request::Parts as RequestParts;
use axum::response::Response as AxumResponse;
use chrono::Utc;
use futures_util::StreamExt;

use crate::{
    event::{EventBus, ProxyEvent, types::RequestEvent},
    protocol::{Request, session::session_id_from_headers},
    proxy2::{
        stage::{
            router_stage::{RouterOutput, RouterStage},
            types::{RequestContext, RequestStage, ResponseStage},
        },
        state::ProxyState,
    },
    timer::Timer,
};

/// Span name set by the pipeline router for the upstream call. Looked up
/// from the per-request `Timer` to populate `RequestEvent.upstream_us`
/// on the orphan-emit path.
const UPSTREAM_SPAN: &str = "Router";

pub struct StagePipeline {
    request_stages: Vec<Box<dyn RequestStage>>,
    response_stages: Arc<Vec<Box<dyn ResponseStage>>>,
    router_stage: RouterStage,
    state: ProxyState,
}

impl StagePipeline {
    pub fn new(
        request_stages: Vec<Box<dyn RequestStage>>,
        response_stages: Vec<Box<dyn ResponseStage>>,
        router_stage: RouterStage,
        state: ProxyState,
    ) -> Self {
        Self {
            request_stages,
            response_stages: Arc::new(response_stages),
            router_stage,
            state,
        }
    }

    /// Entry point after the axum body has been parsed into `Request`.
    ///
    /// The caller's [`Timer`] becomes the context's timer, so spans
    /// tracked outside the pipeline (parse, encode) and the per-stage
    /// spans tracked inside it land in the same dump. For
    /// [`RouterOutput::Stream`], wraps each yielded `Response::Mcp` with
    /// the response stage chain. Stages keep their existing trait and
    /// never see the `Stream` itself.
    pub async fn process(&self, request: Request, timer: Timer) -> anyhow::Result<RouterOutput> {
        let request_ctx = RequestContext::with_timer(&request, timer);
        let result = self.run_stages(request, request_ctx.clone()).await;
        if result.is_err() {
            emit_orphan_event(&request_ctx, &self.state.event_bus);
        }
        result
    }

    async fn run_stages(
        &self,
        mut request: Request,
        request_ctx: RequestContext,
    ) -> anyhow::Result<RouterOutput> {
        for stage in &self.request_stages {
            request = stage
                .process_with_timer(request, request_ctx.clone(), self.state.clone())
                .await?;
        }

        let timer_id = request_ctx.timer.track_start(UPSTREAM_SPAN);
        let output = self
            .router_stage
            .process(request, self.state.clone())
            .await?;
        request_ctx.timer.track_end(timer_id);

        match output {
            RouterOutput::Single(mut response) => {
                for stage in self.response_stages.iter() {
                    response = stage
                        .process_with_timer(response, request_ctx.clone(), self.state.clone())
                        .await?;
                }
                Ok(RouterOutput::Single(response))
            }
            RouterOutput::Stream(parts, stream) => {
                let stages = Arc::clone(&self.response_stages);
                let ctx = request_ctx;
                let state = self.state.clone();
                let mapped = stream.then(move |item| {
                    let stages = Arc::clone(&stages);
                    let ctx = ctx.clone();
                    let state = state.clone();
                    async move {
                        let mut response = item?;
                        for stage in stages.iter() {
                            response = stage
                                .process_with_timer(response, ctx.clone(), state.clone())
                                .await?;
                        }
                        Ok(response)
                    }
                });
                Ok(RouterOutput::Stream(parts, Box::pin(mapped)))
            }
        }
    }

    /// Entry point for `GET` requests with `Accept: text/event-stream`:
    /// open a long-lived upstream SSE channel for this session and stream
    /// it back to the client unmodified. Logs the attach for observability
    /// and skips both stage chains — server-initiated notifications and
    /// `sampling/createMessage` requests don't fit `JsonRpcResult`, and
    /// running existing stages over them would only no-op or misclassify.
    pub async fn process_get_sse(&self, parts: RequestParts) -> anyhow::Result<AxumResponse> {
        match session_id_from_headers(&parts.headers) {
            Some(id) => tracing::info!(session_id = %id, "GET SSE opened"),
            None => tracing::warn!("GET SSE opened without mcp-session-id header"),
        }
        self.router_stage.open_get_sse(parts).await
    }
}

/// Emit a `RequestEvent` with `response: None` for a request whose
/// pipeline failed before the response stage could run. Pulls timing
/// and the snapshotted request from the context. No-op if the context
/// is missing the request snapshot (the `Default` path used by tests
/// that bypass `with_timer`).
fn emit_orphan_event(ctx: &RequestContext, bus: &EventBus) {
    let Some(req_arc) = ctx.request.as_ref() else {
        return;
    };
    let latency_us = u64::try_from(ctx.started_at.elapsed().as_micros()).unwrap_or(u64::MAX);
    let spans = ctx.timer.to_spans_us();
    let upstream_us = spans
        .iter()
        .find(|(name, _)| name == UPSTREAM_SPAN)
        .map(|(_, us)| *us)
        .unwrap_or(0);

    bus.emit(ProxyEvent::Request(Arc::new(RequestEvent {
        request_id: ctx.request_id.clone(),
        request: (**req_arc).clone(),
        response: None,
        ts: Utc::now(),
        latency_us,
        upstream_us,
        spans,
    })));
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;
    use crate::{
        protocol::Response,
        protocol::mcp::{
            ClientMethod, JsonRpcRequest, JsonRpcResult, JsonRpcVersion, RequestId, ToolsMethod,
        },
        proxy2::csp::CspConfig,
        proxy2::{proxy_config::ProxyConfig, state::InnerProxyState},
    };
    use async_trait::async_trait;
    use axum::{Router, body::Bytes as AxumBytes, routing::post};
    use serde_json::{Value, json};
    use std::sync::{Arc, Mutex};

    // ── Test stages ───────────────────────────────────────────

    /// Records its tag in a shared log when invoked. Used to assert order.
    struct TaggedRequestStage {
        log: Arc<Mutex<Vec<&'static str>>>,
        tag: &'static str,
    }

    #[async_trait]
    impl RequestStage for TaggedRequestStage {
        fn name(&self) -> &'static str {
            "TaggedRequestStage"
        }

        async fn process(
            &self,
            req: Request,
            _: RequestContext,
            _: ProxyState,
        ) -> anyhow::Result<Request> {
            self.log.lock().unwrap().push(self.tag);
            Ok(req)
        }
    }

    struct TaggedResponseStage {
        log: Arc<Mutex<Vec<&'static str>>>,
        tag: &'static str,
    }

    #[async_trait]
    impl ResponseStage for TaggedResponseStage {
        fn name(&self) -> &'static str {
            "TaggedResponseStage"
        }

        async fn process(
            &self,
            res: Response,
            _: RequestContext,
            _: ProxyState,
        ) -> anyhow::Result<Response> {
            self.log.lock().unwrap().push(self.tag);
            Ok(res)
        }
    }

    /// Always errors — used to verify request-stage short-circuit.
    struct FailingRequestStage;

    #[async_trait]
    impl RequestStage for FailingRequestStage {
        fn name(&self) -> &'static str {
            "FailingRequestStage"
        }

        async fn process(
            &self,
            _: Request,
            _: RequestContext,
            _: ProxyState,
        ) -> anyhow::Result<Request> {
            Err(anyhow::anyhow!("nope"))
        }
    }

    // ── Helpers ───────────────────────────────────────────────

    fn config_for(url: &str) -> Arc<ProxyConfig> {
        Arc::new(ProxyConfig {
            name: "test".into(),
            mcp: url.to_string(),
            port: None,
            csp: CspConfig::default(),
            max_request_body_size: None,
            max_response_body_size: None,
            max_concurrent_upstream: None,
            connect_timeout: None,
            request_timeout: None,
        })
    }

    fn state() -> ProxyState {
        InnerProxyState::for_tests()
    }

    fn mcp_request() -> Request {
        let parts = axum::http::Request::builder()
            .method("POST")
            .uri("/")
            .body(())
            .unwrap()
            .into_parts()
            .0;
        Request::Mcp(
            parts,
            JsonRpcRequest {
                jsonrpc: JsonRpcVersion,
                id: RequestId::Number(1),
                method: ClientMethod::Tools(ToolsMethod::List),
                params: None,
            },
        )
    }

    /// Spawn an upstream that always returns a JSON-RPC `Response` echoing the id.
    async fn spawn_echo_upstream() -> String {
        async fn echo(body: AxumBytes) -> axum::Json<Value> {
            let req: Value = serde_json::from_slice(&body).unwrap();
            axum::Json(json!({
                "jsonrpc": "2.0",
                "id": req.get("id").cloned().unwrap_or(Value::Null),
                "result": {"ok": true},
            }))
        }
        let app = Router::new().route("/", post(echo));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{addr}")
    }

    // ── process ───────────────────────────────────────────────

    #[tokio::test]
    async fn process__without_stages_runs_router_only() {
        let url = spawn_echo_upstream().await;
        let pipeline = StagePipeline::new(
            vec![],
            vec![],
            RouterStage::new(config_for(&url)).unwrap(),
            state(),
        );

        let resp = pipeline.process(mcp_request(), Timer::new()).await.unwrap();
        let RouterOutput::Single(resp) = resp else {
            panic!("expected Single");
        };
        assert!(matches!(resp, Response::Mcp(_, JsonRpcResult::Response(_))));
    }

    #[tokio::test]
    async fn process__request_stages_run_before_router_in_declared_order() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let url = spawn_echo_upstream().await;
        let stages: Vec<Box<dyn RequestStage>> = vec![
            Box::new(TaggedRequestStage {
                log: log.clone(),
                tag: "first",
            }),
            Box::new(TaggedRequestStage {
                log: log.clone(),
                tag: "second",
            }),
        ];
        let pipeline = StagePipeline::new(
            stages,
            vec![],
            RouterStage::new(config_for(&url)).unwrap(),
            state(),
        );

        pipeline.process(mcp_request(), Timer::new()).await.unwrap();
        assert_eq!(*log.lock().unwrap(), vec!["first", "second"]);
    }

    #[tokio::test]
    async fn process__response_stages_run_after_router_in_declared_order() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let url = spawn_echo_upstream().await;
        let stages: Vec<Box<dyn ResponseStage>> = vec![
            Box::new(TaggedResponseStage {
                log: log.clone(),
                tag: "first",
            }),
            Box::new(TaggedResponseStage {
                log: log.clone(),
                tag: "second",
            }),
        ];
        let pipeline = StagePipeline::new(
            vec![],
            stages,
            RouterStage::new(config_for(&url)).unwrap(),
            state(),
        );

        pipeline.process(mcp_request(), Timer::new()).await.unwrap();
        assert_eq!(*log.lock().unwrap(), vec!["first", "second"]);
    }

    #[tokio::test]
    async fn process__request_stage_error_short_circuits_router() {
        // Upstream URL parses fine but is unreachable — if the router runs
        // it'd fail with a connect error. We expect the stage error first.
        let pipeline = StagePipeline::new(
            vec![Box::new(FailingRequestStage)],
            vec![],
            RouterStage::new(config_for("http://127.0.0.1:1")).unwrap(),
            state(),
        );

        let err = match pipeline.process(mcp_request(), Timer::new()).await {
            Ok(_) => panic!("expected request stage to short-circuit"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("nope"));
    }

    // ── orphan emit on error ──────────────────────────────────

    use crate::event::ProxyEvent as Pe;
    use crate::event::types::RequestEvent as Re;
    use crate::event::{EventBusHandle, EventManager, EventSink};
    use crate::protocol::session::SessionStore;

    #[derive(Clone, Default)]
    struct CapturingSink {
        events: Arc<Mutex<Vec<Pe>>>,
    }

    impl EventSink for CapturingSink {
        fn on_event(&self, event: &Pe) {
            self.events.lock().unwrap().push(event.clone());
        }
        fn name(&self) -> &'static str {
            "capturing"
        }
    }

    fn state_with_sink() -> (ProxyState, CapturingSink, EventBusHandle) {
        let sink = CapturingSink::default();
        let mut mgr = EventManager::new();
        mgr.register(Box::new(sink.clone()));
        let handle = mgr.start();
        let state = Arc::new(InnerProxyState::new(
            handle.bus.clone(),
            SessionStore::new(),
        ));
        (state, sink, handle)
    }

    #[tokio::test]
    async fn process__request_stage_error_emits_orphan_event() {
        let (state, sink, handle) = state_with_sink();
        let pipeline = StagePipeline::new(
            vec![Box::new(FailingRequestStage)],
            vec![],
            RouterStage::new(config_for("http://127.0.0.1:1")).unwrap(),
            state,
        );

        let _ = pipeline.process(mcp_request(), Timer::new()).await;
        handle.shutdown().await;

        let events: Vec<Arc<Re>> = sink
            .events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| match e {
                Pe::Request(re) => Some(re.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(events.len(), 1);
        assert!(events[0].response.is_none());
        assert!(!events[0].request_id.is_empty());
    }

    /// Upstream that returns SSE so RouterOutput::Stream flows through.
    async fn spawn_sse_upstream() -> String {
        async fn sse_handler(body: AxumBytes) -> axum::response::Response {
            let req: Value = serde_json::from_slice(&body).unwrap();
            let id = req.get("id").cloned().unwrap_or(Value::Null);
            let result = serde_json::to_string(&json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {"ok": true},
            }))
            .unwrap();
            // Two frames so the test can assert per-item iteration.
            let body =
                format!("event: message\ndata: {result}\n\nevent: message\ndata: {result}\n\n");
            axum::response::Response::builder()
                .status(200)
                .header("content-type", "text/event-stream")
                .body(axum::body::Body::from(body))
                .unwrap()
        }
        let app = Router::new().route("/", post(sse_handler));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn process__stream_response_runs_stages_per_yielded_item() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let url = spawn_sse_upstream().await;
        let stages: Vec<Box<dyn ResponseStage>> = vec![Box::new(TaggedResponseStage {
            log: log.clone(),
            tag: "stage",
        })];
        let pipeline = StagePipeline::new(
            vec![],
            stages,
            RouterStage::new(config_for(&url)).unwrap(),
            state(),
        );

        let output = pipeline.process(mcp_request(), Timer::new()).await.unwrap();
        let RouterOutput::Stream(_, stream) = output else {
            panic!("expected Stream");
        };
        let mut count = 0;
        futures_util::pin_mut!(stream);
        while let Some(item) = stream.next().await {
            item.unwrap();
            count += 1;
        }
        assert_eq!(count, 2);
        // Stage ran once per yielded item.
        assert_eq!(*log.lock().unwrap(), vec!["stage", "stage"]);
    }
}
