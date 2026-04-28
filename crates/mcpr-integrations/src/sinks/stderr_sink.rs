//! Stderr event sink — prints proxy events to stderr as JSON for real-time visibility.
//!
//! Used in both daemon and foreground modes. Docker/k8s scrape stderr.

use std::io::Write;

use mcpr_core::event::{EventSink, ProxyEvent};
use mcpr_core::protocol::{Request, Response};
use serde_json::json;

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
        ProxyEvent::Request(req) => match req.as_ref() {
            Request::Mcp(rpc) => json!({"type": "request", "kind": "mcp", "rpc": rpc}),
            Request::McpBatch(rpcs) => {
                json!({"type": "request", "kind": "mcp_batch", "rpcs": rpcs})
            }
            Request::Http(http) => json!({
                "type": "request",
                "kind": "http",
                "method": http.method().as_str(),
                "path": http.uri().path(),
                "size": http.body().len(),
            }),
        },
        ProxyEvent::Response(resp) => match resp.as_ref() {
            Response::Mcp(result) => json!({"type": "response", "kind": "mcp", "result": result}),
            Response::McpBatch(rs) => {
                json!({"type": "response", "kind": "mcp_batch", "results": rs})
            }
            Response::Http(http) => json!({
                "type": "response",
                "kind": "http",
                "status": http.status().as_u16(),
                "size": http.body().len(),
            }),
        },
        ProxyEvent::Session(info) => json!({
            "type": "session",
            "id": info.id,
            "state": format!("{:?}", info.state),
            "client_name": info.client_info.as_ref().map(|c| c.name.clone()),
            "client_version": info.client_info.as_ref().and_then(|c| c.version.clone()),
            "request_count": info.request_count,
        }),
    };
    serde_json::to_string(&value).unwrap_or_default()
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;

    use bytes::Bytes;
    use http::{Request as HttpReq, Response as HttpResp, StatusCode};
    use mcpr_core::protocol::mcp::{
        ClientMethod, JsonRpcError, JsonRpcRequest, JsonRpcResponse, JsonRpcResult, JsonRpcVersion,
        RequestId, ToolsMethod,
    };
    use mcpr_core::protocol::session::{ClientInfo, SessionInfo, SessionState};
    use serde_json::{Map, Value, json};

    // ── Helpers ──────────────────────────────────────────────────

    fn mcp_request(method: ClientMethod, params: Option<Map<String, Value>>) -> ProxyEvent {
        ProxyEvent::Request(Box::new(Request::Mcp(JsonRpcRequest {
            jsonrpc: JsonRpcVersion,
            id: RequestId::Number(1),
            method,
            params,
        })))
    }

    fn mcp_response_ok() -> ProxyEvent {
        ProxyEvent::Response(Box::new(Response::Mcp(JsonRpcResult::Response(
            JsonRpcResponse {
                jsonrpc: JsonRpcVersion,
                id: RequestId::Number(1),
                result: Some(json!({"tools": []})),
            },
        ))))
    }

    fn mcp_response_error(code: i32, message: &str) -> ProxyEvent {
        ProxyEvent::Response(Box::new(Response::Mcp(JsonRpcResult::Error(
            JsonRpcError {
                code,
                message: message.into(),
                data: None,
            },
        ))))
    }

    fn http_request(method: &str, path: &str, body: &[u8]) -> ProxyEvent {
        let req = HttpReq::builder()
            .method(method)
            .uri(path)
            .body(Bytes::copy_from_slice(body))
            .unwrap();
        ProxyEvent::Request(Box::new(Request::Http(req)))
    }

    fn http_response(status: u16, body: &[u8]) -> ProxyEvent {
        let resp = HttpResp::builder()
            .status(StatusCode::from_u16(status).unwrap())
            .body(Bytes::copy_from_slice(body))
            .unwrap();
        ProxyEvent::Response(Box::new(Response::Http(resp)))
    }

    fn session(id: &str, state: SessionState, client: Option<ClientInfo>) -> ProxyEvent {
        let mut info = SessionInfo::new(id.into(), state);
        info.client_info = client;
        ProxyEvent::Session(Box::new(info))
    }

    fn render(event: &ProxyEvent) -> Value {
        serde_json::from_str(&StderrSink::new().format_event(event)).unwrap()
    }

    // ── Request ──────────────────────────────────────────────────

    #[test]
    fn json__mcp_request_includes_rpc_envelope() {
        let v = render(&mcp_request(ClientMethod::Tools(ToolsMethod::List), None));
        assert_eq!(v["type"], "request");
        assert_eq!(v["kind"], "mcp");
        assert_eq!(v["rpc"]["method"], "tools/list");
        assert_eq!(v["rpc"]["id"], 1);
    }

    #[test]
    fn json__mcp_request_tools_call_preserves_params() {
        let mut params = Map::new();
        params.insert("name".into(), json!("search"));
        let v = render(&mcp_request(
            ClientMethod::Tools(ToolsMethod::Call),
            Some(params),
        ));
        assert_eq!(v["rpc"]["method"], "tools/call");
        assert_eq!(v["rpc"]["params"]["name"], "search");
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
        let v = render(&ProxyEvent::Request(Box::new(Request::McpBatch(rpcs))));
        assert_eq!(v["kind"], "mcp_batch");
        assert_eq!(v["rpcs"].as_array().unwrap().len(), 2);
        assert_eq!(v["rpcs"][1]["id"], 2);
    }

    #[test]
    fn json__http_request_tagged_with_method_path_and_size() {
        let v = render(&http_request("PUT", "/path", b"hello"));
        assert_eq!(v["type"], "request");
        assert_eq!(v["kind"], "http");
        assert_eq!(v["method"], "PUT");
        assert_eq!(v["path"], "/path");
        assert_eq!(v["size"], 5);
    }

    // ── Response ─────────────────────────────────────────────────

    #[test]
    fn json__mcp_response_ok_serializes_result() {
        let v = render(&mcp_response_ok());
        assert_eq!(v["type"], "response");
        assert_eq!(v["kind"], "mcp");
        assert_eq!(v["result"]["id"], 1);
        assert!(v["result"]["result"]["tools"].is_array());
    }

    #[test]
    fn json__mcp_response_error_includes_code_and_message() {
        let v = render(&mcp_response_error(-32601, "method not found"));
        assert_eq!(v["kind"], "mcp");
        assert_eq!(v["result"]["code"], -32601);
        assert_eq!(v["result"]["message"], "method not found");
    }

    #[test]
    fn json__http_response_includes_status_and_size() {
        let v = render(&http_response(200, b"ok"));
        assert_eq!(v["type"], "response");
        assert_eq!(v["kind"], "http");
        assert_eq!(v["status"], 200);
        assert_eq!(v["size"], 2);
    }

    // ── Session ──────────────────────────────────────────────────

    #[test]
    fn json__session_includes_id_state_and_client() {
        let v = render(&session(
            "sess-1",
            SessionState::Active,
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
        let v = render(&session("sess-2", SessionState::Created, None));
        assert_eq!(v["id"], "sess-2");
        assert_eq!(v["state"], "Created");
        assert!(v["client_name"].is_null());
        assert!(v["client_version"].is_null());
    }
}
