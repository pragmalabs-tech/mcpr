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
