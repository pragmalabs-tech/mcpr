//! Stderr event sink — prints proxy events to stderr as JSON for real-time visibility.
//!
//! Used in both daemon and foreground modes. Docker/k8s scrape stderr.

use std::io::Write;

use chrono::Utc;
use mcpr_core::event::types::{LoggedRequest, LoggedResponse, RequestEvent};
use mcpr_core::event::{EventSink, ProxyEvent};
use mcpr_core::protocol::schema::ChangeSchema;
use serde_json::{Value, json};

/// Sink that prints proxy events to stderr as JSON, one event per line.
pub struct StderrSink;

impl StderrSink {
    pub fn new() -> Self {
        Self
    }

    fn format_event(&self, event: &ProxyEvent) -> String {
        format_json(event)
    }
}

impl Default for StderrSink {
    fn default() -> Self {
        Self::new()
    }
}

impl EventSink for StderrSink {
    fn on_event(&self, event: &ProxyEvent) {
        let line = self.format_event(event);
        let stderr = std::io::stderr();
        let mut handle = stderr.lock();
        let _ = writeln!(handle, "{line}");
    }

    fn flush(&self) {
        let _ = std::io::stderr().flush();
    }

    fn name(&self) -> &'static str {
        "stderr"
    }
}

fn format_json(event: &ProxyEvent) -> String {
    let value = match event {
        ProxyEvent::Request(re) => format_transaction(re),
        ProxyEvent::Session(info) => json!({
            "type": "session",
            "ts": info.last_active.timestamp(),
            "id": info.id,
            "state": format!("{:?}", info.state),
            "client_name": info.client_info.as_ref().map(|c| c.name.clone()),
            "client_version": info.client_info.as_ref().and_then(|c| c.version.clone()),
            "server_name": info.server_info.as_ref().map(|s| s.name.clone()),
            "server_version": info.server_info.as_ref().and_then(|s| s.version.clone()),
            "request_count": info.request_count,
        }),
        ProxyEvent::Schema(change) => {
            let ts = Utc::now().timestamp();
            match change.as_ref() {
                ChangeSchema::Tool(reason, tool) => json!({
                    "type": "schema",
                    "ts": ts,
                    "kind": "tool",
                    "reason": format!("{reason:?}"),
                    "tool": tool,
                }),
                ChangeSchema::Prompt(reason, prompt) => json!({
                    "type": "schema",
                    "ts": ts,
                    "kind": "prompt",
                    "reason": format!("{reason:?}"),
                    "prompt": prompt,
                }),
                ChangeSchema::Resource(reason, resource) => json!({
                    "type": "schema",
                    "ts": ts,
                    "kind": "resource",
                    "reason": format!("{reason:?}"),
                    "resource": resource,
                }),
                ChangeSchema::ResourceTemplate(reason, rt) => json!({
                    "type": "schema",
                    "ts": ts,
                    "kind": "resource_template",
                    "reason": format!("{reason:?}"),
                    "resource_template": rt,
                }),
            }
        }
        ProxyEvent::Heartbeat(hb) => json!({
            "type": "heartbeat",
            "ts": hb.ts.timestamp(),
            "mcp_status": hb.mcp_status,
            "tunnel_status": hb.tunnel_status,
            "tunnel_address": hb.tunnel_address,
            "upstream": hb.upstream,
            "export_port": hb.export_port,
        }),
    };
    serde_json::to_string(&value).unwrap_or_default()
}

fn format_transaction(re: &RequestEvent) -> Value {
    json!({
        "type": "transaction",
        "ts": re.ts.timestamp(),
        "request_id": re.request_id,
        "request": format_request(&re.request),
        "response": re.response.as_ref().map(format_response),
        "latency_us": re.latency_us,
        "upstream_us": re.upstream_us,
    })
}

fn format_request(req: &LoggedRequest) -> Value {
    match req {
        LoggedRequest::Mcp(_, rpc) => json!({"kind": "mcp", "rpc": rpc}),
        LoggedRequest::McpBatch(_, rpcs) => json!({"kind": "mcp_batch", "rpcs": rpcs}),
        LoggedRequest::Http {
            method,
            uri,
            body_size,
        } => json!({
            "kind": "http",
            "method": method.as_str(),
            "path": uri.path(),
            "size": body_size,
        }),
    }
}

fn format_response(resp: &LoggedResponse) -> Value {
    match resp {
        LoggedResponse::Mcp(_, result) => json!({"kind": "mcp", "result": result}),
        LoggedResponse::McpBatch(_, rs) => json!({"kind": "mcp_batch", "results": rs}),
        LoggedResponse::Http { status, body_size } => json!({
            "kind": "http",
            "status": status.as_u16(),
            "size": body_size,
        }),
    }
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;

    use std::sync::Arc;

    use chrono::Utc;
    use http::{Method, Request as HttpReq, Response as HttpResp, StatusCode, Uri};
    use mcpr_core::protocol::mcp::{
        ClientInfo, ClientMethod, JsonRpcError, JsonRpcErrorResponse, JsonRpcRequest,
        JsonRpcResponse, JsonRpcResult, JsonRpcVersion, RequestId, ToolsMethod,
    };
    use mcpr_core::protocol::session::SessionInfo;
    use serde_json::{Map, Value, json};

    // ── Helpers ──────────────────────────────────────────────────

    fn empty_request_parts() -> http::request::Parts {
        HttpReq::builder()
            .method("POST")
            .uri("/")
            .body(())
            .unwrap()
            .into_parts()
            .0
    }

    fn empty_response_parts() -> http::response::Parts {
        HttpResp::builder().body(()).unwrap().into_parts().0
    }

    fn transaction(request: LoggedRequest, response: LoggedResponse) -> ProxyEvent {
        ProxyEvent::Request(Arc::new(RequestEvent {
            request_id: "rid-1".into(),
            request,
            response: Some(response),
            ts: Utc::now(),
            latency_us: 0,
            upstream_us: 0,
            spans: vec![],
        }))
    }

    fn orphan(request: LoggedRequest) -> ProxyEvent {
        ProxyEvent::Request(Arc::new(RequestEvent {
            request_id: "rid-orphan".into(),
            request,
            response: None,
            ts: Utc::now(),
            latency_us: 100,
            upstream_us: 0,
            spans: vec![],
        }))
    }

    fn mcp_request(method: ClientMethod, params: Option<Map<String, Value>>) -> LoggedRequest {
        LoggedRequest::Mcp(
            empty_request_parts(),
            JsonRpcRequest {
                jsonrpc: JsonRpcVersion,
                id: RequestId::Number(1),
                method,
                params,
            },
        )
    }

    fn mcp_response_ok() -> LoggedResponse {
        LoggedResponse::Mcp(
            empty_response_parts(),
            JsonRpcResult::Response(JsonRpcResponse {
                jsonrpc: JsonRpcVersion,
                id: RequestId::Number(1),
                result: Some(json!({"tools": []})),
            }),
        )
    }

    fn mcp_response_error(code: i32, message: &str) -> LoggedResponse {
        LoggedResponse::Mcp(
            empty_response_parts(),
            JsonRpcResult::Error(JsonRpcErrorResponse {
                jsonrpc: JsonRpcVersion,
                id: RequestId::Number(1),
                error: JsonRpcError {
                    code,
                    message: message.into(),
                    data: None,
                },
            }),
        )
    }

    fn http_request(method: &str, path: &str, body: &[u8]) -> LoggedRequest {
        LoggedRequest::Http {
            method: Method::from_bytes(method.as_bytes()).unwrap(),
            uri: path.parse::<Uri>().unwrap(),
            body_size: body.len(),
        }
    }

    fn http_response(status: u16, body: &[u8]) -> LoggedResponse {
        LoggedResponse::Http {
            status: StatusCode::from_u16(status).unwrap(),
            body_size: body.len(),
        }
    }

    fn session(id: &str, client: Option<ClientInfo>) -> ProxyEvent {
        let info = SessionInfo::new(id.into(), client, RequestId::Number(1));
        ProxyEvent::Session(Arc::new(info))
    }

    fn render(event: &ProxyEvent) -> Value {
        serde_json::from_str(&StderrSink::new().format_event(event)).unwrap()
    }

    // ── Transaction shape ─────────────────────────────────────────

    #[test]
    fn json__transaction_includes_request_id() {
        let v = render(&transaction(
            mcp_request(ClientMethod::Tools(ToolsMethod::List), None),
            mcp_response_ok(),
        ));
        assert_eq!(v["type"], "transaction");
        assert_eq!(v["request_id"], "rid-1");
    }

    #[test]
    fn json__transaction_carries_latency_fields() {
        let event = ProxyEvent::Request(Arc::new(RequestEvent {
            request_id: "rid-2".into(),
            request: mcp_request(ClientMethod::Tools(ToolsMethod::List), None),
            response: Some(mcp_response_ok()),
            ts: Utc::now(),
            latency_us: 1234,
            upstream_us: 999,
            spans: vec![],
        }));
        let v = render(&event);
        assert_eq!(v["latency_us"], 1234);
        assert_eq!(v["upstream_us"], 999);
    }

    #[test]
    fn json__transaction_emits_ts_in_seconds_from_event() {
        let ts = chrono::DateTime::from_timestamp(1_700_000_123, 0).unwrap();
        let event = ProxyEvent::Request(Arc::new(RequestEvent {
            request_id: "rid-3".into(),
            request: mcp_request(ClientMethod::Tools(ToolsMethod::List), None),
            response: Some(mcp_response_ok()),
            ts,
            latency_us: 0,
            upstream_us: 0,
            spans: vec![],
        }));
        let v = render(&event);
        assert_eq!(v["ts"], 1_700_000_123);
    }

    #[test]
    fn json__heartbeat_emits_ts_in_seconds_from_event() {
        let ts = chrono::DateTime::from_timestamp(1_700_000_456, 0).unwrap();
        let event = ProxyEvent::Heartbeat(Arc::new(mcpr_core::event::types::HeartbeatEvent {
            mcp_status: "ok".into(),
            tunnel_status: "off".into(),
            tunnel_address: None,
            upstream: "http://localhost:9000".into(),
            export_port: 9002,
            ts,
        }));
        let v = render(&event);
        assert_eq!(v["type"], "heartbeat");
        assert_eq!(v["ts"], 1_700_000_456);
    }

    #[test]
    fn json__session_emits_ts_in_seconds() {
        let v = render(&session("sess-ts", None));
        assert!(v["ts"].is_i64());
    }

    #[test]
    fn json__schema_emits_ts_in_seconds() {
        use mcpr_core::protocol::schema::{Reason, Tool};
        let event = ProxyEvent::Schema(Arc::new(ChangeSchema::Tool(
            Reason::Added,
            Tool {
                name: "search".into(),
                title: None,
                description: None,
                input_schema: json!({"type": "object"}),
                output_schema: None,
                annotations: None,
                meta: None,
            },
        )));
        let v = render(&event);
        assert_eq!(v["type"], "schema");
        assert!(v["ts"].is_i64());
    }

    #[test]
    fn json__orphan_transaction_emits_null_response() {
        let v = render(&orphan(mcp_request(
            ClientMethod::Tools(ToolsMethod::List),
            None,
        )));
        assert_eq!(v["type"], "transaction");
        assert_eq!(v["request_id"], "rid-orphan");
        assert!(v["response"].is_null());
        assert_eq!(v["latency_us"], 100);
    }

    // ── Request side of the transaction ──────────────────────────

    #[test]
    fn json__mcp_request_includes_rpc_envelope() {
        let v = render(&transaction(
            mcp_request(ClientMethod::Tools(ToolsMethod::List), None),
            mcp_response_ok(),
        ));
        assert_eq!(v["request"]["kind"], "mcp");
        assert_eq!(v["request"]["rpc"]["method"], "tools/list");
        assert_eq!(v["request"]["rpc"]["id"], 1);
    }

    #[test]
    fn json__mcp_request_tools_call_preserves_params() {
        let mut params = Map::new();
        params.insert("name".into(), json!("search"));
        let v = render(&transaction(
            mcp_request(ClientMethod::Tools(ToolsMethod::Call), Some(params)),
            mcp_response_ok(),
        ));
        assert_eq!(v["request"]["rpc"]["method"], "tools/call");
        assert_eq!(v["request"]["rpc"]["params"]["name"], "search");
    }

    #[test]
    fn json__mcp_batch_request_includes_each_rpc() {
        let rpcs = vec![
            JsonRpcRequest {
                jsonrpc: JsonRpcVersion,
                id: RequestId::Number(1),
                method: ClientMethod::Tools(ToolsMethod::List),
                params: None,
            },
            JsonRpcRequest {
                jsonrpc: JsonRpcVersion,
                id: RequestId::Number(2),
                method: ClientMethod::Tools(ToolsMethod::List),
                params: None,
            },
        ];
        let v = render(&transaction(
            LoggedRequest::McpBatch(empty_request_parts(), rpcs),
            mcp_response_ok(),
        ));
        assert_eq!(v["request"]["kind"], "mcp_batch");
        assert_eq!(v["request"]["rpcs"].as_array().unwrap().len(), 2);
        assert_eq!(v["request"]["rpcs"][1]["id"], 2);
    }

    #[test]
    fn json__http_request_tagged_with_method_path_and_size() {
        let v = render(&transaction(
            http_request("PUT", "/path", b"hello"),
            http_response(200, b"ok"),
        ));
        assert_eq!(v["request"]["kind"], "http");
        assert_eq!(v["request"]["method"], "PUT");
        assert_eq!(v["request"]["path"], "/path");
        assert_eq!(v["request"]["size"], 5);
    }

    // ── Response side of the transaction ─────────────────────────

    #[test]
    fn json__mcp_response_ok_serializes_result() {
        let v = render(&transaction(
            mcp_request(ClientMethod::Tools(ToolsMethod::List), None),
            mcp_response_ok(),
        ));
        assert_eq!(v["response"]["kind"], "mcp");
        assert_eq!(v["response"]["result"]["id"], 1);
        assert!(v["response"]["result"]["result"]["tools"].is_array());
    }

    #[test]
    fn json__mcp_response_error_includes_code_and_message() {
        let v = render(&transaction(
            mcp_request(ClientMethod::Tools(ToolsMethod::List), None),
            mcp_response_error(-32601, "method not found"),
        ));
        assert_eq!(v["response"]["kind"], "mcp");
        assert_eq!(v["response"]["result"]["error"]["code"], -32601);
        assert_eq!(
            v["response"]["result"]["error"]["message"],
            "method not found"
        );
    }

    #[test]
    fn json__http_response_includes_status_and_size() {
        let v = render(&transaction(
            http_request("GET", "/", b""),
            http_response(200, b"ok"),
        ));
        assert_eq!(v["response"]["kind"], "http");
        assert_eq!(v["response"]["status"], 200);
        assert_eq!(v["response"]["size"], 2);
    }

    // ── Session ──────────────────────────────────────────────────

    #[test]
    fn json__session_includes_id_state_and_client() {
        let v = render(&session(
            "sess-1",
            Some(ClientInfo {
                name: "cursor".into(),
                version: None,
            }),
        ));
        assert_eq!(v["type"], "session");
        assert_eq!(v["id"], "sess-1");
        assert_eq!(v["state"], "Active");
        assert_eq!(v["client_name"], "cursor");
        assert!(v["client_version"].is_null());
    }

    #[test]
    fn json__session_without_client_emits_nulls() {
        let v = render(&session("sess-2", None));
        assert_eq!(v["id"], "sess-2");
        assert_eq!(v["state"], "Active");
        assert!(v["client_name"].is_null());
        assert!(v["client_version"].is_null());
    }
}
