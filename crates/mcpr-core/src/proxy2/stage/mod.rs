//! Request → router → response stage chain. Request stages mutate
//! inbound traffic in order, `RouterStage` talks to upstream, response
//! stages mutate outbound traffic in order on the way back.

use crate::{
    protocol::{Request, Response},
    proxy2::{
        stage::{
            router_stage::RouterStage,
            types::{RequestStage, ResponseStage},
        },
        state::ProxyState,
    },
};

pub mod log_stage;
pub mod router_stage;
pub mod types;

pub struct StagePipeline {
    request_stages: Vec<Box<dyn RequestStage>>,
    response_stages: Vec<Box<dyn ResponseStage>>,
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
            response_stages,
            router_stage,
            state,
        }
    }

    /// Entry point after the axum body has been parsed into `Request`.
    pub async fn process(&self, mut request: Request) -> anyhow::Result<Response> {
        for stage in &self.request_stages {
            request = stage.process(request, self.state.clone()).await?;
        }

        let mut response = self
            .router_stage
            .process(request, self.state.clone())
            .await?;

        for stage in &self.response_stages {
            response = stage.process(response, self.state.clone()).await?;
        }

        Ok(response)
    }
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;
    use crate::{
        csp::CspConfig,
        protocol::mcp::{
            ClientMethod, JsonRpcRequest, JsonRpcResult, JsonRpcVersion, RequestId, ToolsMethod,
        },
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
        async fn process(&self, req: Request, _: ProxyState) -> anyhow::Result<Request> {
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
        async fn process(&self, res: Response, _: ProxyState) -> anyhow::Result<Response> {
            self.log.lock().unwrap().push(self.tag);
            Ok(res)
        }
    }

    /// Always errors — used to verify request-stage short-circuit.
    struct FailingRequestStage;

    #[async_trait]
    impl RequestStage for FailingRequestStage {
        async fn process(&self, _: Request, _: ProxyState) -> anyhow::Result<Request> {
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

        let resp = pipeline.process(mcp_request()).await.unwrap();
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

        pipeline.process(mcp_request()).await.unwrap();
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

        pipeline.process(mcp_request()).await.unwrap();
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

        let err = match pipeline.process(mcp_request()).await {
            Ok(_) => panic!("expected request stage to short-circuit"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("nope"));
    }
}
