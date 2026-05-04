//! Proxy event types carried through the event bus.
//!
//! [`ProxyEvent`] is the single event enum; sinks match on the variant to
//! decide what to do with each event. Variants hold their payload behind
//! `Arc` so fan-out to multiple sinks is a refcount bump rather than a
//! deep clone.

use std::collections::HashMap;
use std::sync::Arc;

use axum::http::{
    Method, StatusCode, Uri, request::Parts as RequestParts, response::Parts as ResponseParts,
};
use chrono::{DateTime, Utc};
use serde_json::{Map, Value};

use crate::event::openai::OpenAiClientContext;
use crate::protocol::{
    Request, Response,
    mcp::{ClientMethod, JsonRpcRequest, JsonRpcResult, RequestId, ResourcesMethod},
    schema::ChangeSchema,
    session::SessionInfo,
};

/// Logging projection of [`Request`]. MCP variants carry the full
/// JSON-RPC envelope and inbound HTTP `Parts`; the HTTP variant keeps
/// only routing-level metadata so non-MCP traffic (HTML pages, SSE
/// streams, health probes) does not bloat sinks.
#[derive(Clone)]
pub enum LoggedRequest {
    Mcp(RequestParts, JsonRpcRequest),
    McpBatch(RequestParts, Vec<JsonRpcRequest>),
    Http {
        method: Method,
        uri: Uri,
        body_size: usize,
    },
}

impl From<&Request> for LoggedRequest {
    fn from(req: &Request) -> Self {
        match req {
            Request::Mcp(parts, rpc) => Self::Mcp(parts.clone(), rpc.clone()),
            Request::McpBatch(parts, rpcs) => Self::McpBatch(parts.clone(), rpcs.clone()),
            Request::Http(http) => Self::Http {
                method: http.method().clone(),
                uri: http.uri().clone(),
                body_size: http.body().len(),
            },
        }
    }
}

/// Logging projection of [`Response`]. Mirrors [`LoggedRequest`]: MCP
/// keeps the full JSON-RPC result and upstream `Parts`; HTTP carries
/// only the status and body size.
#[derive(Clone)]
pub enum LoggedResponse {
    Mcp(ResponseParts, JsonRpcResult),
    McpBatch(ResponseParts, Vec<JsonRpcResult>),
    Http {
        status: StatusCode,
        body_size: usize,
    },
}

impl From<&Response> for LoggedResponse {
    fn from(resp: &Response) -> Self {
        match resp {
            Response::Mcp(parts, result) => Self::Mcp(parts.clone(), result.clone()),
            Response::McpBatch(parts, results) => Self::McpBatch(parts.clone(), results.clone()),
            Response::Http(http) => Self::Http {
                status: http.status(),
                body_size: http.body().len(),
            },
        }
    }
}

impl LoggedResponse {
    /// Strip resource bodies from MCP `resources/{list,templates/list,read}`
    /// results so events carry only the naming/path fields. Widget HTML and
    /// blob bodies returned by `resources/read` can be megabytes apiece, and
    /// list/templates entries carry descriptions and metadata sinks don't
    /// need. Logging projection only - the response sent to the client is
    /// untouched (this stage runs after the client copy has already left).
    pub fn slim_resources_in_place(&mut self, methods: &HashMap<RequestId, ClientMethod>) {
        match self {
            Self::Mcp(_, result) => slim_result(result, methods),
            Self::McpBatch(_, results) => {
                for r in results {
                    slim_result(r, methods);
                }
            }
            Self::Http { .. } => {}
        }
    }
}

fn slim_result(result: &mut JsonRpcResult, methods: &HashMap<RequestId, ClientMethod>) {
    let JsonRpcResult::Response(resp) = result else {
        return;
    };
    let Some(method) = methods.get(&resp.id) else {
        return;
    };
    let Some(value) = resp.result.as_mut() else {
        return;
    };
    match method {
        ClientMethod::Resources(ResourcesMethod::List) => {
            slim_array(value, "resources", &["uri", "name"]);
        }
        ClientMethod::Resources(ResourcesMethod::TemplatesList) => {
            slim_array(value, "resourceTemplates", &["uriTemplate", "name"]);
        }
        ClientMethod::Resources(ResourcesMethod::Read) => {
            slim_array(value, "contents", &["uri"]);
        }
        _ => {}
    }
}

/// Replace `result` with `{ array_key: [{ keep_field: value, ... }, ...] }`,
/// dropping every other top-level field (e.g. `nextCursor`, `_meta`) and
/// every per-item field outside `keep`.
fn slim_array(result: &mut Value, array_key: &str, keep: &[&str]) {
    let items = result
        .as_object_mut()
        .and_then(|obj| obj.remove(array_key))
        .and_then(|v| match v {
            Value::Array(arr) => Some(arr),
            _ => None,
        })
        .unwrap_or_default();

    let slimmed: Vec<Value> = items
        .into_iter()
        .map(|item| match item {
            Value::Object(obj) => {
                let mut out = Map::new();
                for k in keep {
                    if let Some(v) = obj.get(*k) {
                        out.insert((*k).to_string(), v.clone());
                    }
                }
                Value::Object(out)
            }
            _ => Value::Object(Map::new()),
        })
        .collect();

    let mut out = Map::new();
    out.insert(array_key.to_string(), Value::Array(slimmed));
    *result = Value::Object(out);
}

/// One full request/response transaction, emitted once after the
/// response stage completes (`response: Some(...)`) or once on the
/// error path when the response stage never ran (`response: None`).
///
/// - `request_id` is the correlation key sinks should use to join the
///   request and response halves. Source depends on the variant:
///   MCP requests use the JSON-RPC `id` (stringified); HTTP requests
///   get a fresh UUID v4 minted at pipeline entry. MCP batch (legacy,
///   removed from spec in 2025-06-18) falls back to the first rpc's id.
/// - `request` and `response` are the logging projections of what the
///   pipeline saw. HTTP traffic is reduced to method/uri/status/size;
///   MCP traffic carries the full JSON-RPC envelope. `response` is
///   `None` for orphan transactions: parse-pass-but-pipeline-fail,
///   client disconnects mid-request, request stage errors, etc.
/// - `latency_us` / `upstream_us` are the headline numbers extracted
///   from the per-request timer; `spans` is the full span snapshot.
///   `upstream_us` is 0 when the router never ran (orphan).
/// - `ts` is captured at emit time so cloud-sink batching delay doesn't
///   skew analytics.
/// - `openai` is `Some` when the inbound request carried `_meta.openai/*`
///   keys (ChatGPT Apps SDK / Connectors). `None` for every other client.
///   See [`OpenAiClientContext`] for the field-by-field mapping.
#[derive(Clone)]
pub struct RequestEvent {
    pub request_id: String,
    pub request: LoggedRequest,
    pub response: Option<LoggedResponse>,
    pub ts: DateTime<Utc>,
    pub latency_us: u64,
    pub upstream_us: u64,
    pub spans: Vec<(String, u64)>,
    pub openai: Option<OpenAiClientContext>,
}

/// Periodic snapshot of a proxy's runtime status. Emitted on a fixed
/// cadence by the CLI host (not by the request pipeline) so the cloud
/// can answer "is this server up and where does it live?" without a
/// dedicated control-plane connection.
#[derive(Clone)]
pub struct HeartbeatEvent {
    pub mcp_status: String,
    pub tunnel_status: String,
    pub tunnel_address: Option<String>,
    pub upstream: String,
    pub export_port: u16,
    pub ts: DateTime<Utc>,
}

/// All events flowing through the event bus.
///
/// Each variant represents a distinct lifecycle moment. Sinks match on
/// the variant to decide what to process.
#[derive(Clone)]
pub enum ProxyEvent {
    Request(Arc<RequestEvent>),
    Session(Arc<SessionInfo>),
    Schema(Arc<ChangeSchema>),
    Heartbeat(Arc<HeartbeatEvent>),
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;
    use crate::protocol::mcp::{
        JsonRpcError, JsonRpcErrorResponse, JsonRpcResponse, JsonRpcVersion, ToolsMethod,
    };
    use serde_json::json;

    fn empty_response_parts() -> ResponseParts {
        axum::http::Response::new(()).into_parts().0
    }

    fn methods_for(id: i64, method: ClientMethod) -> HashMap<RequestId, ClientMethod> {
        let mut m = HashMap::new();
        m.insert(RequestId::Number(id), method);
        m
    }

    fn mcp_response(id: i64, result: Value) -> LoggedResponse {
        LoggedResponse::Mcp(
            empty_response_parts(),
            JsonRpcResult::Response(JsonRpcResponse {
                jsonrpc: JsonRpcVersion,
                id: RequestId::Number(id),
                result: Some(result),
            }),
        )
    }

    fn extract_result(resp: &LoggedResponse) -> &Value {
        match resp {
            LoggedResponse::Mcp(_, JsonRpcResult::Response(r)) => r.result.as_ref().unwrap(),
            _ => panic!("expected Mcp response"),
        }
    }

    #[test]
    fn slim_resources_in_place__resources_list_keeps_only_uri_and_name() {
        let mut resp = mcp_response(
            1,
            json!({
                "resources": [{
                    "uri": "ui://widget/a",
                    "name": "Widget A",
                    "title": "the title",
                    "description": "long description",
                    "mimeType": "text/html",
                    "size": 12345,
                    "_meta": { "openai/widgetCSP": { "connect_domains": ["x"] } }
                }],
                "nextCursor": "cursor-token"
            }),
        );

        resp.slim_resources_in_place(&methods_for(
            1,
            ClientMethod::Resources(ResourcesMethod::List),
        ));

        let result = extract_result(&resp);
        assert_eq!(
            result,
            &json!({
                "resources": [{ "uri": "ui://widget/a", "name": "Widget A" }]
            })
        );
    }

    #[test]
    fn slim_resources_in_place__resources_templates_list_keeps_uri_template_and_name() {
        let mut resp = mcp_response(
            1,
            json!({
                "resourceTemplates": [{
                    "uriTemplate": "ui://widget/{name}.html",
                    "name": "Widget Template",
                    "description": "drop me",
                    "_meta": { "x": 1 }
                }]
            }),
        );

        resp.slim_resources_in_place(&methods_for(
            1,
            ClientMethod::Resources(ResourcesMethod::TemplatesList),
        ));

        assert_eq!(
            extract_result(&resp),
            &json!({
                "resourceTemplates": [{
                    "uriTemplate": "ui://widget/{name}.html",
                    "name": "Widget Template"
                }]
            })
        );
    }

    #[test]
    fn slim_resources_in_place__resources_read_drops_html_text_and_meta() {
        let mut resp = mcp_response(
            1,
            json!({
                "contents": [{
                    "uri": "ui://widget/q",
                    "mimeType": "text/html",
                    "text": "<html><body>huge widget body</body></html>",
                    "_meta": { "openai/widgetDomain": "old.example.com" }
                }]
            }),
        );

        resp.slim_resources_in_place(&methods_for(
            1,
            ClientMethod::Resources(ResourcesMethod::Read),
        ));

        assert_eq!(
            extract_result(&resp),
            &json!({ "contents": [{ "uri": "ui://widget/q" }] })
        );
    }

    #[test]
    fn slim_resources_in_place__resources_read_drops_blob_payload() {
        let mut resp = mcp_response(
            1,
            json!({
                "contents": [{
                    "uri": "file:///x.bin",
                    "mimeType": "application/octet-stream",
                    "blob": "AAAA....base64....=="
                }]
            }),
        );

        resp.slim_resources_in_place(&methods_for(
            1,
            ClientMethod::Resources(ResourcesMethod::Read),
        ));

        assert_eq!(
            extract_result(&resp),
            &json!({ "contents": [{ "uri": "file:///x.bin" }] })
        );
    }

    #[test]
    fn slim_resources_in_place__non_resource_method_is_untouched() {
        let original = json!({
            "tools": [{
                "name": "search",
                "description": "kept",
                "inputSchema": { "type": "object" }
            }]
        });
        let mut resp = mcp_response(1, original.clone());

        resp.slim_resources_in_place(&methods_for(1, ClientMethod::Tools(ToolsMethod::List)));

        assert_eq!(extract_result(&resp), &original);
    }

    #[test]
    fn slim_resources_in_place__error_response_is_untouched() {
        let mut resp = LoggedResponse::Mcp(
            empty_response_parts(),
            JsonRpcResult::Error(JsonRpcErrorResponse {
                jsonrpc: JsonRpcVersion,
                id: RequestId::Number(1),
                error: JsonRpcError {
                    code: -32603,
                    message: "boom".into(),
                    data: None,
                },
            }),
        );

        resp.slim_resources_in_place(&methods_for(
            1,
            ClientMethod::Resources(ResourcesMethod::Read),
        ));

        let LoggedResponse::Mcp(_, JsonRpcResult::Error(e)) = &resp else {
            panic!("expected error variant");
        };
        assert_eq!(e.error.code, -32603);
    }

    #[test]
    fn slim_resources_in_place__id_with_no_method_mapping_is_untouched() {
        let original = json!({
            "contents": [{ "uri": "u", "text": "<html/>" }]
        });
        let mut resp = mcp_response(1, original.clone());

        resp.slim_resources_in_place(&HashMap::new());

        assert_eq!(extract_result(&resp), &original);
    }

    #[test]
    fn slim_resources_in_place__missing_array_yields_empty_array() {
        let mut resp = mcp_response(1, json!({ "nextCursor": "abc" }));

        resp.slim_resources_in_place(&methods_for(
            1,
            ClientMethod::Resources(ResourcesMethod::List),
        ));

        assert_eq!(extract_result(&resp), &json!({ "resources": [] }));
    }

    #[test]
    fn slim_resources_in_place__http_variant_is_noop() {
        let mut resp = LoggedResponse::Http {
            status: StatusCode::OK,
            body_size: 42,
        };
        resp.slim_resources_in_place(&methods_for(
            1,
            ClientMethod::Resources(ResourcesMethod::Read),
        ));
        let LoggedResponse::Http { status, body_size } = resp else {
            panic!("expected Http variant");
        };
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body_size, 42);
    }

    #[test]
    fn slim_resources_in_place__batch_slims_each_item_by_its_method() {
        let mut methods = HashMap::new();
        methods.insert(
            RequestId::Number(1),
            ClientMethod::Resources(ResourcesMethod::Read),
        );
        methods.insert(RequestId::Number(2), ClientMethod::Tools(ToolsMethod::List));

        let mut resp = LoggedResponse::McpBatch(
            empty_response_parts(),
            vec![
                JsonRpcResult::Response(JsonRpcResponse {
                    jsonrpc: JsonRpcVersion,
                    id: RequestId::Number(1),
                    result: Some(json!({
                        "contents": [{
                            "uri": "ui://w",
                            "mimeType": "text/html",
                            "text": "<html/>"
                        }]
                    })),
                }),
                JsonRpcResult::Response(JsonRpcResponse {
                    jsonrpc: JsonRpcVersion,
                    id: RequestId::Number(2),
                    result: Some(json!({ "tools": [{ "name": "t" }] })),
                }),
            ],
        );

        resp.slim_resources_in_place(&methods);

        let LoggedResponse::McpBatch(_, items) = &resp else {
            panic!("expected McpBatch");
        };
        let JsonRpcResult::Response(r0) = &items[0] else {
            panic!()
        };
        let JsonRpcResult::Response(r1) = &items[1] else {
            panic!()
        };
        assert_eq!(
            r0.result.as_ref().unwrap(),
            &json!({ "contents": [{ "uri": "ui://w" }] })
        );
        assert_eq!(
            r1.result.as_ref().unwrap(),
            &json!({ "tools": [{ "name": "t" }] })
        );
    }
}
