/// Stage will handle a piece logic of Proxy Stage
/// It can
///     1. Edit Current request
///     2. Do noning and foward
///     3. Return immediate
///
use async_trait::async_trait;

pub enum StageAction {
    Continue(crate::protocol::Request),
    Return(crate::protocol::Response),
}

#[async_trait]
pub trait ProxyStage {
    async fn process(
        &self,
        request: crate::protocol::Request,
        state: &crate::proxy2::state::ProxyState,
    ) -> StageAction;
}
