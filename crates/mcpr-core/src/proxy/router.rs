//! Pure `(Request, Config) -> Route` mapping. No I/O.
//!
//! Owns the `BufferPolicy` table — buffering is a routing decision,
//! not an intrinsic method property, so it lives here rather than on
//! the method enum.

use super::pipeline::driver::Router;
use super::pipeline::stubs::UrlMap;
use super::pipeline::values::{BufferPolicy, Context, McpTransport, Request, Route};
use crate::protocol::mcp::{
    ClientKind, ClientMethod, LifecycleMethod, PromptsMethod, ResourcesMethod, ToolsMethod,
};

pub struct ProxyRouter;

impl Router for ProxyRouter {
    fn route(&self, req: &Request, cx: &Context) -> Route {
        let upstream = cx.intake.proxy.mcp_upstream.clone();

        match req {
            Request::Mcp(mcp) => match mcp.transport {
                McpTransport::StreamableHttpPost | McpTransport::StreamableHttpGet => {
                    let method = match &mcp.kind {
                        ClientKind::Request(m) => m.clone(),
                        _ => ClientMethod::Unknown("notification-or-reply".into()),
                    };
                    let buffer_policy =
                        buffer_policy_for(&method, cx.intake.proxy.max_response_body);
                    Route::McpStreamableHttp {
                        upstream,
                        method,
                        buffer_policy,
                    }
                }
                McpTransport::SseLegacyGet => Route::McpSseLegacy { upstream },
            },
            Request::OAuth(_) => Route::Oauth {
                upstream,
                rewrite: UrlMap,
            },
            Request::Raw(_) => Route::Raw { upstream },
        }
    }
}

/// The buffer-policy table. These 7 methods get their responses parsed
/// so response middlewares can mutate them (schema ingest, CSP rewrite).
/// Every other method streams bytes through untouched.
fn buffer_policy_for(method: &ClientMethod, max: usize) -> BufferPolicy {
    match method {
        ClientMethod::Lifecycle(LifecycleMethod::Initialize)
        | ClientMethod::Tools(ToolsMethod::List)
        | ClientMethod::Tools(ToolsMethod::Call)
        | ClientMethod::Resources(ResourcesMethod::List)
        | ClientMethod::Resources(ResourcesMethod::TemplatesList)
        | ClientMethod::Resources(ResourcesMethod::Read)
        | ClientMethod::Prompts(PromptsMethod::List) => BufferPolicy::Buffered { max },
        _ => BufferPolicy::Streamed,
    }
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;

    use axum::body::Body;
    use axum::http::{HeaderMap, Method};
    use serde_json::Value;

    use crate::protocol::mcp::{CompletionMethod, LoggingMethod, TasksMethod};
    use crate::proxy::pipeline::middlewares::test_support::{
        mcp_request, test_context, test_proxy_state,
    };
    use crate::proxy::pipeline::values::{RawRequest, Request};

    const DEFAULT_MAX: usize = 1 << 20;

    #[test]
    fn buffer_policy_for__seven_buffered_methods_match_legacy_table() {
        let buffered = [
            ClientMethod::Lifecycle(LifecycleMethod::Initialize),
            ClientMethod::Tools(ToolsMethod::List),
            ClientMethod::Tools(ToolsMethod::Call),
            ClientMethod::Resources(ResourcesMethod::List),
            ClientMethod::Resources(ResourcesMethod::TemplatesList),
            ClientMethod::Resources(ResourcesMethod::Read),
            ClientMethod::Prompts(PromptsMethod::List),
        ];
        for m in buffered {
            assert!(
                matches!(buffer_policy_for(&m, DEFAULT_MAX), BufferPolicy::Buffered { max } if max == DEFAULT_MAX),
                "method {m:?} should buffer"
            );
        }
    }

    #[test]
    fn buffer_policy_for__streamed_methods() {
        let streamed = [
            ClientMethod::Ping,
            ClientMethod::Prompts(PromptsMethod::Get),
            ClientMethod::Resources(ResourcesMethod::Subscribe),
            ClientMethod::Resources(ResourcesMethod::Unsubscribe),
            ClientMethod::Completion(CompletionMethod::Complete),
            ClientMethod::Logging(LoggingMethod::SetLevel),
            ClientMethod::Tasks(TasksMethod::List),
            ClientMethod::Tasks(TasksMethod::Get),
            ClientMethod::Tasks(TasksMethod::Result),
            ClientMethod::Tasks(TasksMethod::Cancel),
        ];
        for m in streamed {
            assert_eq!(
                buffer_policy_for(&m, DEFAULT_MAX),
                BufferPolicy::Streamed,
                "method {m:?} should stream"
            );
        }
    }

    #[test]
    fn buffer_policy_for__unknown_is_streamed() {
        assert_eq!(
            buffer_policy_for(&ClientMethod::Unknown("x".into()), DEFAULT_MAX),
            BufferPolicy::Streamed,
        );
    }

    #[tokio::test]
    async fn route__mcp_post_tools_list_is_buffered_streamable_http() {
        let proxy = test_proxy_state();
        let cx = test_context(proxy);
        let req = mcp_request("tools/list", Value::Null, None);
        match ProxyRouter.route(&req, &cx) {
            Route::McpStreamableHttp {
                method,
                buffer_policy,
                ..
            } => {
                assert!(matches!(method, ClientMethod::Tools(ToolsMethod::List)));
                assert!(matches!(buffer_policy, BufferPolicy::Buffered { .. }));
            }
            other => panic!("expected streamable http, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn route__mcp_post_ping_is_streamed_streamable_http() {
        let proxy = test_proxy_state();
        let cx = test_context(proxy);
        let req = mcp_request("ping", Value::Null, None);
        match ProxyRouter.route(&req, &cx) {
            Route::McpStreamableHttp { buffer_policy, .. } => {
                assert_eq!(buffer_policy, BufferPolicy::Streamed);
            }
            other => panic!("expected streamable http, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn route__mcp_notification_is_streamed() {
        let proxy = test_proxy_state();
        let cx = test_context(proxy);
        // `mcp_request` hard-codes id=1, so build a notification inline.
        use crate::protocol::jsonrpc::JsonRpcEnvelope;
        use crate::protocol::mcp::ClientNotifMethod;
        use crate::proxy::pipeline::values::{McpRequest, McpTransport};
        let envelope =
            JsonRpcEnvelope::parse(br#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
                .unwrap();
        let req = Request::Mcp(McpRequest {
            transport: McpTransport::StreamableHttpPost,
            envelope,
            kind: ClientKind::Notification(ClientNotifMethod::Initialized),
            headers: HeaderMap::new(),
            session_hint: None,
        });
        match ProxyRouter.route(&req, &cx) {
            Route::McpStreamableHttp { buffer_policy, .. } => {
                assert_eq!(buffer_policy, BufferPolicy::Streamed);
            }
            other => panic!("expected streamable http, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn route__sse_legacy_intake_is_sse_legacy_route() {
        use crate::protocol::jsonrpc::JsonRpcEnvelope;
        use crate::protocol::mcp::ClientNotifMethod;
        use crate::proxy::pipeline::values::{McpRequest, McpTransport};
        let proxy = test_proxy_state();
        let cx = test_context(proxy);
        let envelope = JsonRpcEnvelope::parse(br#"{"jsonrpc":"2.0","method":"ping"}"#).unwrap();
        let req = Request::Mcp(McpRequest {
            transport: McpTransport::SseLegacyGet,
            envelope,
            kind: ClientKind::Notification(ClientNotifMethod::Unknown("ping".into())),
            headers: HeaderMap::new(),
            session_hint: None,
        });
        assert!(matches!(
            ProxyRouter.route(&req, &cx),
            Route::McpSseLegacy { .. }
        ));
    }

    #[tokio::test]
    async fn route__raw_is_raw_route() {
        let proxy = test_proxy_state();
        let cx = test_context(proxy);
        let req = Request::Raw(RawRequest {
            method: Method::GET,
            path: "/health".into(),
            body: Body::empty(),
            headers: HeaderMap::new(),
        });
        assert!(matches!(ProxyRouter.route(&req, &cx), Route::Raw { .. }));
    }

    #[tokio::test]
    async fn route__propagates_upstream_string_from_state() {
        let proxy = test_proxy_state();
        let cx = test_context(proxy);
        let req = mcp_request("tools/list", Value::Null, None);
        match ProxyRouter.route(&req, &cx) {
            Route::McpStreamableHttp { upstream, .. } => {
                assert_eq!(upstream, "http://upstream.test");
            }
            _ => panic!("expected streamable http"),
        }
    }
}
