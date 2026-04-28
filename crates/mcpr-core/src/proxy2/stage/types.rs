//! Stage traits — pre/post hooks around the router. Each stage can
//! mutate the value, leave it untouched, or fail the pipeline by
//! returning an error.

use async_trait::async_trait;

use crate::{
    protocol::{Request, Response},
    proxy2::state::ProxyState,
};

#[async_trait]
pub trait RequestStage: Send + Sync {
    async fn process(&self, request: Request, state: ProxyState) -> anyhow::Result<Request>;
}

#[async_trait]
pub trait ResponseStage: Send + Sync {
    async fn process(&self, res: Response, state: ProxyState) -> anyhow::Result<Response>;
}
