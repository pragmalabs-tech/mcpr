//! Stage traits — pre/post hooks around the router. Each stage can
//! mutate the value, leave it untouched, or fail the pipeline by
//! returning an error.
//!
//! Both stage kinds receive a [`RequestContext`] alongside the value:
//! a `RequestId → ClientMethod` map built once at pipeline entry from
//! the parsed [`Request`]. Stages that need to dispatch on the
//! originating MCP method read from the map; stages that don't ignore
//! it. HTTP requests produce an empty map.

use std::collections::HashMap;

use async_trait::async_trait;

use crate::{
    protocol::{
        Request, Response,
        mcp::{ClientMethod, RequestId},
    },
    proxy2::state::ProxyState,
};

/// Per-request metadata threaded through both stage chains. Cheap to
/// clone (one `HashMap` clone per stage call); the map has 1 entry for
/// single MCP requests, N entries for batches, and is empty for HTTP.
#[derive(Clone, Debug, Default)]
pub struct RequestContext {
    pub client_methods: HashMap<RequestId, ClientMethod>,
}

impl RequestContext {
    /// Build a context from a parsed `Request`. HTTP requests carry no
    /// MCP method, so the map is empty.
    pub fn from_request(request: &Request) -> Self {
        let client_methods = match request {
            Request::Mcp(_, rpc) => {
                let mut m = HashMap::with_capacity(1);
                m.insert(rpc.id.clone(), rpc.method.clone());
                m
            }
            Request::McpBatch(_, rpcs) => rpcs
                .iter()
                .map(|r| (r.id.clone(), r.method.clone()))
                .collect(),
            Request::Http(_) => HashMap::new(),
        };
        Self { client_methods }
    }

    pub fn get_method(&self, request_id: &RequestId) -> Option<&ClientMethod> {
        self.client_methods.get(request_id)
    }
}

#[async_trait]
pub trait RequestStage: Send + Sync {
    async fn process(
        &self,
        request: Request,
        request_ctx: RequestContext,
        state: ProxyState,
    ) -> anyhow::Result<Request>;
}

#[async_trait]
pub trait ResponseStage: Send + Sync {
    async fn process(
        &self,
        response: Response,
        request_ctx: RequestContext,
        state: ProxyState,
    ) -> anyhow::Result<Response>;
}
