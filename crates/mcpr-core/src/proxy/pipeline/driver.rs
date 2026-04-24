//! Pipeline driver — the engine that runs middleware chains, the
//! router, and the transport.
//!
//! See `PIPELINE_ARCHITECTURE.md` §Driver. A short explicit loop that
//! owns an ordered `Vec<Box<dyn …>>` for each chain. No tower, no
//! service combinators.

use async_trait::async_trait;

use super::middleware::{Flow, RequestMiddleware, ResponseMiddleware};
use super::values::{Context, Request, Response, Route};

/// Pure function: decide where a request is headed. No I/O.
pub trait Router: Send + Sync {
    fn route(&self, req: &Request, cx: &Context) -> Route;
}

/// The one layer that touches the network. Reqwest errors become
/// `Response::Upstream502`.
#[async_trait]
pub trait Transport: Send + Sync {
    async fn dispatch(&self, req: Request, route: Route, cx: &Context) -> Response;
}

pub struct Pipeline<R: Router, T: Transport> {
    request_chain: Vec<Box<dyn RequestMiddleware>>,
    response_chain: Vec<Box<dyn ResponseMiddleware>>,
    router: R,
    transport: T,
}

impl<R: Router, T: Transport> Pipeline<R, T> {
    pub fn new(
        request_chain: Vec<Box<dyn RequestMiddleware>>,
        response_chain: Vec<Box<dyn ResponseMiddleware>>,
        router: R,
        transport: T,
    ) -> Self {
        // Registration logging is handled by `build_default_pipeline`,
        // which owns construction ordering and is the single site where
        // operator-visible chain composition needs to be reported.
        Self {
            request_chain,
            response_chain,
            router,
            transport,
        }
    }

    pub fn request_chain_names(&self) -> Vec<&'static str> {
        self.request_chain.iter().map(|mw| mw.name()).collect()
    }

    pub fn response_chain_names(&self) -> Vec<&'static str> {
        self.response_chain.iter().map(|mw| mw.name()).collect()
    }

    pub async fn run(&self, req: Request, cx: &mut Context) -> Response {
        let resp = match self.run_request_chain(req, cx).await {
            Ok(req) => {
                let route = self.router.route(&req, cx);
                self.transport.dispatch(req, route, cx).await
            }
            Err(short) => short,
        };
        self.run_response_chain(resp, cx).await
    }

    async fn run_request_chain(
        &self,
        mut req: Request,
        cx: &mut Context,
    ) -> Result<Request, Response> {
        for mw in &self.request_chain {
            match mw.on_request(req, cx).await {
                Flow::Continue(r) => req = r,
                Flow::ShortCircuit(r) => return Err(r),
            }
        }
        Ok(req)
    }

    async fn run_response_chain(&self, mut resp: Response, cx: &mut Context) -> Response {
        for mw in &self.response_chain {
            resp = mw.on_response(resp, cx).await;
        }
        resp
    }
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use std::sync::{Arc, Mutex};

    use axum::http::{HeaderMap, StatusCode};
    use serde_json::json;

    use super::*;
    use crate::proxy::pipeline::envelope::JsonRpcEnvelope;
    use crate::proxy::pipeline::message::{
        ClientKind, ClientMethod, McpMessage, MessageKind, ToolsMethod,
    };
    use crate::proxy::pipeline::middleware::{Flow, RequestMiddleware, ResponseMiddleware};
    use crate::proxy::pipeline::middlewares::test_support::{test_context, test_proxy_state};
    use crate::proxy::pipeline::values::{
        BufferPolicy, Envelope, McpRequest, McpTransport, Request, Response, Route,
    };

    // ── Fakes ────────────────────────────────────────────────

    enum FakeReqAction {
        Continue,
        AnnotateTag(&'static str),
        ShortCircuit(&'static str),
    }

    struct FakeReqMw {
        name: &'static str,
        action: FakeReqAction,
    }

    #[async_trait]
    impl RequestMiddleware for FakeReqMw {
        fn name(&self) -> &'static str {
            self.name
        }
        async fn on_request(&self, req: Request, cx: &mut Context) -> Flow {
            match &self.action {
                FakeReqAction::Continue => Flow::Continue(req),
                FakeReqAction::AnnotateTag(t) => {
                    cx.working.tags.push(t);
                    Flow::Continue(req)
                }
                FakeReqAction::ShortCircuit(reason) => Flow::ShortCircuit(Response::Upstream502 {
                    reason: (*reason).to_owned(),
                }),
            }
        }
    }

    struct FakeRespMw {
        name: &'static str,
        annotate: &'static str,
    }

    #[async_trait]
    impl ResponseMiddleware for FakeRespMw {
        fn name(&self) -> &'static str {
            self.name
        }
        async fn on_response(&self, resp: Response, cx: &mut Context) -> Response {
            cx.working.tags.push(self.annotate);
            resp
        }
    }

    struct FakeRouter {
        route: Mutex<Option<Route>>,
        calls: Arc<Mutex<u32>>,
    }

    impl Router for FakeRouter {
        fn route(&self, _req: &Request, _cx: &Context) -> Route {
            *self.calls.lock().unwrap() += 1;
            self.route
                .lock()
                .unwrap()
                .take()
                .expect("FakeRouter called more than once")
        }
    }

    struct FakeTransport {
        response: Mutex<Option<Response>>,
        calls: Arc<Mutex<u32>>,
    }

    #[async_trait]
    impl Transport for FakeTransport {
        async fn dispatch(&self, _req: Request, _route: Route, _cx: &Context) -> Response {
            *self.calls.lock().unwrap() += 1;
            self.response
                .lock()
                .unwrap()
                .take()
                .expect("FakeTransport called more than once")
        }
    }

    // ── Harness ─────────────────────────────────────────────

    fn stub_mcp_request() -> Request {
        let env =
            JsonRpcEnvelope::parse(br#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#).unwrap();
        Request::Mcp(McpRequest {
            transport: McpTransport::StreamableHttpPost,
            envelope: env,
            kind: ClientKind::Request(ClientMethod::Tools(ToolsMethod::List)),
            headers: HeaderMap::new(),
            session_hint: None,
        })
    }

    fn stub_buffered_response() -> Response {
        let env =
            JsonRpcEnvelope::parse(br#"{"jsonrpc":"2.0","id":1,"result":{"tools":[]}}"#).unwrap();
        let message = McpMessage {
            envelope: env,
            kind: MessageKind::Server(crate::proxy::pipeline::message::ServerKind::Result),
        };
        Response::McpBuffered {
            envelope: Envelope::Json,
            message,
            status: StatusCode::OK,
            headers: HeaderMap::new(),
        }
    }

    fn stub_route() -> Route {
        Route::McpStreamableHttp {
            upstream: "http://upstream.test/mcp".into(),
            method: ClientMethod::Tools(ToolsMethod::List),
            buffer_policy: BufferPolicy::Buffered { max: 4096 },
        }
    }

    // ── Tests ───────────────────────────────────────────────

    #[tokio::test]
    async fn run__empty_chain_returns_transport_response() {
        let proxy = test_proxy_state();
        let mut cx = test_context(proxy);
        let router_calls = Arc::new(Mutex::new(0));
        let transport_calls = Arc::new(Mutex::new(0));
        let pipeline = Pipeline::new(
            Vec::<Box<dyn RequestMiddleware>>::new(),
            Vec::<Box<dyn ResponseMiddleware>>::new(),
            FakeRouter {
                route: Mutex::new(Some(stub_route())),
                calls: router_calls.clone(),
            },
            FakeTransport {
                response: Mutex::new(Some(stub_buffered_response())),
                calls: transport_calls.clone(),
            },
        );

        let resp = pipeline.run(stub_mcp_request(), &mut cx).await;
        assert!(matches!(resp, Response::McpBuffered { .. }));
        assert_eq!(*router_calls.lock().unwrap(), 1);
        assert_eq!(*transport_calls.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn run__request_chain_fires_in_order() {
        let proxy = test_proxy_state();
        let mut cx = test_context(proxy);
        let pipeline = Pipeline::new(
            vec![
                Box::new(FakeReqMw {
                    name: "a",
                    action: FakeReqAction::AnnotateTag("tag-a"),
                }) as _,
                Box::new(FakeReqMw {
                    name: "b",
                    action: FakeReqAction::AnnotateTag("tag-b"),
                }) as _,
                Box::new(FakeReqMw {
                    name: "c",
                    action: FakeReqAction::AnnotateTag("tag-c"),
                }) as _,
            ],
            Vec::<Box<dyn ResponseMiddleware>>::new(),
            FakeRouter {
                route: Mutex::new(Some(stub_route())),
                calls: Arc::new(Mutex::new(0)),
            },
            FakeTransport {
                response: Mutex::new(Some(stub_buffered_response())),
                calls: Arc::new(Mutex::new(0)),
            },
        );
        pipeline.run(stub_mcp_request(), &mut cx).await;
        assert_eq!(cx.working.tags.as_slice(), &["tag-a", "tag-b", "tag-c"]);
    }

    #[tokio::test]
    async fn run__short_circuit_skips_router_transport_and_later_request_mws() {
        let proxy = test_proxy_state();
        let mut cx = test_context(proxy);
        let router_calls = Arc::new(Mutex::new(0));
        let transport_calls = Arc::new(Mutex::new(0));
        let pipeline = Pipeline::new(
            vec![
                Box::new(FakeReqMw {
                    name: "before",
                    action: FakeReqAction::AnnotateTag("before"),
                }) as _,
                Box::new(FakeReqMw {
                    name: "cut",
                    action: FakeReqAction::ShortCircuit("cut"),
                }) as _,
                Box::new(FakeReqMw {
                    name: "after",
                    action: FakeReqAction::AnnotateTag("after"),
                }) as _,
            ],
            Vec::<Box<dyn ResponseMiddleware>>::new(),
            FakeRouter {
                route: Mutex::new(Some(stub_route())),
                calls: router_calls.clone(),
            },
            FakeTransport {
                response: Mutex::new(Some(stub_buffered_response())),
                calls: transport_calls.clone(),
            },
        );

        let resp = pipeline.run(stub_mcp_request(), &mut cx).await;
        assert!(matches!(resp, Response::Upstream502 { .. }));
        assert_eq!(cx.working.tags.as_slice(), &["before"]);
        assert_eq!(*router_calls.lock().unwrap(), 0);
        assert_eq!(*transport_calls.lock().unwrap(), 0);
    }

    #[tokio::test]
    async fn run__response_chain_runs_after_short_circuit() {
        let proxy = test_proxy_state();
        let mut cx = test_context(proxy);
        let pipeline = Pipeline::new(
            vec![Box::new(FakeReqMw {
                name: "cut",
                action: FakeReqAction::ShortCircuit("x"),
            }) as _],
            vec![
                Box::new(FakeRespMw {
                    name: "r1",
                    annotate: "resp-1",
                }) as _,
                Box::new(FakeRespMw {
                    name: "r2",
                    annotate: "resp-2",
                }) as _,
            ],
            FakeRouter {
                route: Mutex::new(Some(stub_route())),
                calls: Arc::new(Mutex::new(0)),
            },
            FakeTransport {
                response: Mutex::new(Some(stub_buffered_response())),
                calls: Arc::new(Mutex::new(0)),
            },
        );

        pipeline.run(stub_mcp_request(), &mut cx).await;
        assert_eq!(cx.working.tags.as_slice(), &["resp-1", "resp-2"]);
    }

    #[tokio::test]
    async fn run__response_chain_folds_in_order() {
        let proxy = test_proxy_state();
        let mut cx = test_context(proxy);
        let pipeline = Pipeline::new(
            Vec::<Box<dyn RequestMiddleware>>::new(),
            vec![
                Box::new(FakeRespMw {
                    name: "r1",
                    annotate: "r1",
                }) as _,
                Box::new(FakeRespMw {
                    name: "r2",
                    annotate: "r2",
                }) as _,
                Box::new(FakeRespMw {
                    name: "r3",
                    annotate: "r3",
                }) as _,
            ],
            FakeRouter {
                route: Mutex::new(Some(stub_route())),
                calls: Arc::new(Mutex::new(0)),
            },
            FakeTransport {
                response: Mutex::new(Some(stub_buffered_response())),
                calls: Arc::new(Mutex::new(0)),
            },
        );
        pipeline.run(stub_mcp_request(), &mut cx).await;
        assert_eq!(cx.working.tags.as_slice(), &["r1", "r2", "r3"]);
    }

    #[tokio::test]
    async fn chain_names__reports_registered_middlewares() {
        let pipeline = Pipeline::new(
            vec![
                Box::new(FakeReqMw {
                    name: "session_touch",
                    action: FakeReqAction::Continue,
                }) as _,
                Box::new(FakeReqMw {
                    name: "client_info_inject",
                    action: FakeReqAction::Continue,
                }) as _,
            ],
            vec![
                Box::new(FakeRespMw {
                    name: "schema_ingest",
                    annotate: "",
                }) as _,
                Box::new(FakeRespMw {
                    name: "envelope_seal",
                    annotate: "",
                }) as _,
            ],
            FakeRouter {
                route: Mutex::new(Some(stub_route())),
                calls: Arc::new(Mutex::new(0)),
            },
            FakeTransport {
                response: Mutex::new(Some(stub_buffered_response())),
                calls: Arc::new(Mutex::new(0)),
            },
        );
        assert_eq!(
            pipeline.request_chain_names(),
            vec!["session_touch", "client_info_inject"],
        );
        assert_eq!(
            pipeline.response_chain_names(),
            vec!["schema_ingest", "envelope_seal"],
        );
    }

    // ── Smoke test — one request through a full stub chain ──

    #[tokio::test]
    async fn smoke__request_response_roundtrip_with_mutation() {
        let proxy = test_proxy_state();
        let mut cx = test_context(proxy);
        let pipeline = Pipeline::new(
            vec![Box::new(FakeReqMw {
                name: "tag",
                action: FakeReqAction::AnnotateTag("touched"),
            }) as _],
            vec![Box::new(FakeRespMw {
                name: "tag_resp",
                annotate: "sealed",
            }) as _],
            FakeRouter {
                route: Mutex::new(Some(stub_route())),
                calls: Arc::new(Mutex::new(0)),
            },
            FakeTransport {
                response: Mutex::new(Some(Response::McpBuffered {
                    envelope: Envelope::Json,
                    message: McpMessage {
                        envelope: JsonRpcEnvelope::parse(
                            br#"{"jsonrpc":"2.0","id":42,"result":{"tools":[]}}"#,
                        )
                        .unwrap(),
                        kind: MessageKind::Server(
                            crate::proxy::pipeline::message::ServerKind::Result,
                        ),
                    },
                    status: StatusCode::OK,
                    headers: HeaderMap::new(),
                })),
                calls: Arc::new(Mutex::new(0)),
            },
        );

        let resp = pipeline.run(stub_mcp_request(), &mut cx).await;
        match resp {
            Response::McpBuffered {
                status, message, ..
            } => {
                assert_eq!(status, StatusCode::OK);
                let result: serde_json::Value = message
                    .envelope
                    .result_as()
                    .expect("result should deserialize");
                assert_eq!(result, json!({"tools": []}));
            }
            other => panic!("expected McpBuffered, got {other:?}"),
        }
        assert_eq!(cx.working.tags.as_slice(), &["touched", "sealed"]);
    }
}
