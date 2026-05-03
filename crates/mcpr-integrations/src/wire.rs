//! Wire envelope for `ProxyEvent` → cloud ingest.
//!
//! The cloud ingest endpoint receives an array of these envelopes and
//! lands them verbatim in `events_raw`. A ClickHouse materialized view
//! does the analytics projection on read.
//!
//! ```text
//! { "v": 1, "ts": "...", "server": "<slug>", "kind": "...", "payload": { ... } }
//! ```

use mcpr_core::event::ProxyEvent;
use mcpr_core::event::types::HeartbeatEvent;
use mcpr_core::protocol::mcp::{JsonRpcRequest, JsonRpcResult, RequestId};
use mcpr_core::protocol::schema::{ChangeSchema, Reason};
use mcpr_core::protocol::session::{SessionInfo, SessionState, session_id_from_headers};
use mcpr_core::protocol::{Request, Response};
use mcpr_core::timer::Timer;
use serde_json::{Map, Value, json};

const ENVELOPE_VERSION: u8 = 1;

/// Build the JSON envelope for one event. The cloud ingest accepts
/// `Vec<Envelope>` as the request body.
pub fn encode_envelope(event: &ProxyEvent, server: &str) -> Value {
    let (kind, ts, payload) = match event {
        ProxyEvent::Request(re) => (
            "request",
            re.ts.to_rfc3339(),
            encode_request(&re.request, &re.request_id),
        ),
        ProxyEvent::Response(re) => (
            "response",
            re.ts.to_rfc3339(),
            encode_response(&re.response, &re.request_id, re.latency_us, &re.timer),
        ),
        ProxyEvent::Session(info) => (
            "session",
            info.last_active.to_rfc3339(),
            encode_session(info),
        ),
        ProxyEvent::Schema(change) => (
            "schema",
            chrono::Utc::now().to_rfc3339(),
            encode_schema(change),
        ),
        ProxyEvent::Heartbeat(hb) => ("heartbeat", hb.ts.to_rfc3339(), encode_heartbeat(hb)),
    };

    json!({
        "v": ENVELOPE_VERSION,
        "ts": ts,
        "server": server,
        "kind": kind,
        "payload": payload,
    })
}

fn encode_request(req: &Request, request_id: &str) -> Value {
    match req {
        Request::Mcp(parts, rpc) => {
            let session_id = session_id_from_headers(&parts.headers);
            let mut payload = base_request_payload(request_id, session_id.as_deref(), rpc);
            payload["request_size"] = json!(0);
            payload
        }
        Request::McpBatch(parts, rpcs) => {
            let session_id = session_id_from_headers(&parts.headers);
            // Batches collapse to a synthetic envelope with the first
            // RPC's method as the discriminator. Per-RPC analytics rows
            // are produced by unfolding in the cloud if/when needed.
            let first = rpcs.first();
            let mcp_method = first.and_then(|r| r.method.as_str()).unwrap_or("");
            json!({
                "request_id": request_id,
                "session_id": session_id,
                "mcp_method": mcp_method,
                "batch_size": rpcs.len(),
            })
        }
        Request::Http(http) => {
            let session_id = session_id_from_headers(http.headers());
            json!({
                "request_id": request_id,
                "session_id": session_id,
                "http_method": http.method().as_str(),
                "path": http.uri().path(),
                "request_size": http.body().len() as u64,
            })
        }
    }
}

fn base_request_payload(request_id: &str, session_id: Option<&str>, rpc: &JsonRpcRequest) -> Value {
    let mcp_method = rpc.method.as_str().unwrap_or("");
    json!({
        "request_id": request_id,
        "rpc_id": request_id_to_value(&rpc.id),
        "session_id": session_id,
        "mcp_method": mcp_method,
        "tool": rpc.get_tool().unwrap_or(""),
        "resource_uri": rpc.get_resource_uri().unwrap_or(""),
        "prompt_name": rpc.get_prompt().unwrap_or(""),
    })
}

fn encode_response(resp: &Response, request_id: &str, latency_us: u64, timer: &Timer) -> Value {
    let timer_value = encode_timer(timer);
    match resp {
        Response::Mcp(parts, result) => {
            let http_status = parts.status.as_u16();
            let (status, error_code, error_detail) = mcp_result_status(result);
            json!({
                "request_id": request_id,
                "latency_us": latency_us,
                "upstream_us": 0,
                "http_status": http_status,
                "status": status,
                "error_code": error_code,
                "error_detail": error_detail,
                "response_size": 0,
                "timer": timer_value,
            })
        }
        Response::McpBatch(parts, results) => {
            let http_status = parts.status.as_u16();
            // Batch status is "ok" if every result succeeded, else "error".
            let any_error = results.iter().any(|r| matches!(r, JsonRpcResult::Error(_)));
            let status = if any_error { "error" } else { "ok" };
            json!({
                "request_id": request_id,
                "latency_us": latency_us,
                "http_status": http_status,
                "status": status,
                "batch_size": results.len(),
                "response_size": 0,
                "timer": timer_value,
            })
        }
        Response::Http(http) => {
            let http_status = http.status().as_u16();
            let status = if http.status().is_success() {
                "ok"
            } else {
                "error"
            };
            json!({
                "request_id": request_id,
                "latency_us": latency_us,
                "http_status": http_status,
                "status": status,
                "response_size": http.body().len() as u64,
                "timer": timer_value,
            })
        }
    }
}

/// Collapse the timer's spans into a flat `{name: duration_us}` object.
/// Stages that ran more than once on a single request (e.g. response
/// stages on each frame of a stream) have their durations summed so the
/// shape stays comparable across requests.
fn encode_timer(timer: &Timer) -> Value {
    let mut map: Map<String, Value> = Map::new();
    for (name, us) in timer.to_spans_us() {
        let acc = map
            .get(&name)
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
            .saturating_add(us);
        map.insert(name, json!(acc));
    }
    Value::Object(map)
}

fn mcp_result_status(result: &JsonRpcResult) -> (&'static str, String, String) {
    match result {
        JsonRpcResult::Response(_) => ("ok", String::new(), String::new()),
        JsonRpcResult::Error(err) => (
            "error",
            err.error.code.to_string(),
            err.error.message.clone(),
        ),
    }
}

fn encode_session(info: &SessionInfo) -> Value {
    let state = match info.state {
        SessionState::Active => "active",
        SessionState::Closed => "closed",
    };
    json!({
        "session_id": info.id,
        "state": state,
        "client_name": info.client_info.as_ref().map(|c| c.name.as_str()),
        "client_version": info.client_info.as_ref().and_then(|c| c.version.as_deref()),
        "server_name": info.server_info.as_ref().map(|s| s.name.as_str()),
        "server_version": info.server_info.as_ref().and_then(|s| s.version.as_deref()),
        "created_at": info.created_at.to_rfc3339(),
        "last_active": info.last_active.to_rfc3339(),
        "request_count": info.request_count,
    })
}

fn encode_schema(change: &ChangeSchema) -> Value {
    let reason_str = |r: Reason| match r {
        Reason::Added => "added",
        Reason::Observed => "observed",
    };
    match change {
        ChangeSchema::Tool(reason, tool) => json!({
            "kind": "tool",
            "reason": reason_str(*reason),
            "name": tool.name,
            "tool": tool,
        }),
        ChangeSchema::Prompt(reason, prompt) => json!({
            "kind": "prompt",
            "reason": reason_str(*reason),
            "name": prompt.name,
            "prompt": prompt,
        }),
        ChangeSchema::Resource(reason, resource) => json!({
            "kind": "resource",
            "reason": reason_str(*reason),
            "uri": resource.uri,
            "resource": resource,
        }),
        ChangeSchema::ResourceTemplate(reason, rt) => json!({
            "kind": "resource_template",
            "reason": reason_str(*reason),
            "uri_template": rt.uri_template,
            "resource_template": rt,
        }),
    }
}

fn encode_heartbeat(hb: &HeartbeatEvent) -> Value {
    json!({
        "mcp_status": hb.mcp_status,
        "tunnel_status": hb.tunnel_status,
        "tunnel_address": hb.tunnel_address,
        "upstream": hb.upstream,
        "export_port": hb.export_port,
    })
}

fn request_id_to_value(id: &RequestId) -> Value {
    match id {
        RequestId::Number(n) => json!(*n),
        RequestId::String(s) => json!(s),
        RequestId::Null => Value::Null,
    }
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;

    use std::sync::Arc;

    use bytes::Bytes;
    use chrono::Utc;
    use http::{Request as HttpReq, Response as HttpResp, StatusCode};
    use mcpr_core::event::{RequestEvent, ResponseEvent};
    use mcpr_core::protocol::mcp::{
        ClientInfo, ClientMethod, JsonRpcError, JsonRpcErrorResponse, JsonRpcRequest,
        JsonRpcResponse, JsonRpcResult, JsonRpcVersion, PromptsMethod, RequestId, ResourcesMethod,
        ToolsMethod,
    };
    use mcpr_core::protocol::schema::{Prompt, Resource, ResourceTemplate, Tool, ToolAnnotations};
    use serde_json::{Map, json};

    // ── Helpers ──────────────────────────────────────────────────

    fn req_parts(session_id: Option<&str>) -> http::request::Parts {
        let mut b = HttpReq::builder().method("POST").uri("/");
        if let Some(sid) = session_id {
            b = b.header("mcp-session-id", sid);
        }
        b.body(()).unwrap().into_parts().0
    }

    fn resp_parts(status: u16) -> http::response::Parts {
        HttpResp::builder()
            .status(StatusCode::from_u16(status).unwrap())
            .body(())
            .unwrap()
            .into_parts()
            .0
    }

    fn rpc(method: ClientMethod, params: Option<Map<String, Value>>) -> JsonRpcRequest {
        JsonRpcRequest {
            jsonrpc: JsonRpcVersion,
            id: RequestId::Number(1),
            method,
            params,
        }
    }

    fn request_event(req: Request) -> ProxyEvent {
        ProxyEvent::Request(Arc::new(RequestEvent {
            request: req,
            request_id: "req-abc".into(),
            ts: Utc::now(),
        }))
    }

    fn response_event(resp: Response, latency_us: u64) -> ProxyEvent {
        ProxyEvent::Response(Arc::new(ResponseEvent {
            response: resp,
            request_id: "req-abc".into(),
            latency_us,
            timer: Timer::default(),
            ts: Utc::now(),
        }))
    }

    fn tool(name: &str) -> Tool {
        Tool {
            name: name.into(),
            title: None,
            description: None,
            input_schema: json!({"type": "object"}),
            output_schema: None,
            annotations: Some(ToolAnnotations::default()),
            meta: None,
        }
    }

    // ── encode_envelope (top-level shape) ────────────────────────

    #[test]
    fn encode_envelope__top_level_fields_present() {
        let event = request_event(Request::Mcp(
            req_parts(None),
            rpc(ClientMethod::Tools(ToolsMethod::List), None),
        ));
        let v = encode_envelope(&event, "prod-server");

        assert_eq!(v["v"], json!(ENVELOPE_VERSION));
        assert_eq!(v["server"], "prod-server");
        assert_eq!(v["kind"], "request");
        assert!(v["ts"].is_string());
        assert!(v["payload"].is_object());
    }

    #[test]
    fn encode_envelope__server_slug_passed_through_unchanged() {
        let event = request_event(Request::Mcp(
            req_parts(None),
            rpc(ClientMethod::Tools(ToolsMethod::List), None),
        ));
        assert_eq!(encode_envelope(&event, "")["server"], "");
        assert_eq!(
            encode_envelope(&event, "weird-Slug_42")["server"],
            "weird-Slug_42"
        );
    }

    // ── encode_request: Mcp ──────────────────────────────────────

    #[test]
    fn encode_request__mcp_carries_request_id_and_method() {
        let event = request_event(Request::Mcp(
            req_parts(None),
            rpc(ClientMethod::Tools(ToolsMethod::List), None),
        ));
        let p = encode_envelope(&event, "s")["payload"].clone();

        assert_eq!(p["request_id"], "req-abc");
        assert_eq!(p["mcp_method"], "tools/list");
    }

    #[test]
    fn encode_request__mcp_tools_call_extracts_tool_name() {
        let mut params = Map::new();
        params.insert("name".into(), json!("search"));
        let event = request_event(Request::Mcp(
            req_parts(None),
            rpc(ClientMethod::Tools(ToolsMethod::Call), Some(params)),
        ));
        let p = encode_envelope(&event, "s")["payload"].clone();

        assert_eq!(p["tool"], "search");
        assert_eq!(p["mcp_method"], "tools/call");
    }

    #[test]
    fn encode_request__mcp_resources_read_extracts_uri() {
        let mut params = Map::new();
        params.insert("uri".into(), json!("file:///doc.md"));
        let event = request_event(Request::Mcp(
            req_parts(None),
            rpc(ClientMethod::Resources(ResourcesMethod::Read), Some(params)),
        ));
        let p = encode_envelope(&event, "s")["payload"].clone();

        assert_eq!(p["resource_uri"], "file:///doc.md");
    }

    #[test]
    fn encode_request__mcp_prompts_get_extracts_prompt_name() {
        let mut params = Map::new();
        params.insert("name".into(), json!("greeting"));
        let event = request_event(Request::Mcp(
            req_parts(None),
            rpc(ClientMethod::Prompts(PromptsMethod::Get), Some(params)),
        ));
        let p = encode_envelope(&event, "s")["payload"].clone();

        assert_eq!(p["prompt_name"], "greeting");
    }

    #[test]
    fn encode_request__mcp_session_id_from_inbound_header() {
        let event = request_event(Request::Mcp(
            req_parts(Some("sess-1")),
            rpc(ClientMethod::Tools(ToolsMethod::List), None),
        ));
        let p = encode_envelope(&event, "s")["payload"].clone();

        assert_eq!(p["session_id"], "sess-1");
    }

    #[test]
    fn encode_request__mcp_session_id_null_when_header_missing() {
        let event = request_event(Request::Mcp(
            req_parts(None),
            rpc(ClientMethod::Tools(ToolsMethod::List), None),
        ));
        let p = encode_envelope(&event, "s")["payload"].clone();

        assert!(p["session_id"].is_null());
    }

    #[test]
    fn encode_request__mcp_unknown_method_serializes_as_empty_string() {
        let event = request_event(Request::Mcp(
            req_parts(None),
            rpc(ClientMethod::Unknown("custom/thing".into()), None),
        ));
        let p = encode_envelope(&event, "s")["payload"].clone();

        // Unknown methods are not in the enum's strum names, so as_str()
        // returns None and the encoder falls back to "".
        assert_eq!(p["mcp_method"], "");
    }

    // ── encode_request: McpBatch ─────────────────────────────────

    #[test]
    fn encode_request__mcp_batch_carries_size_and_first_method() {
        let batch = Request::McpBatch(
            req_parts(Some("sess-b")),
            vec![
                rpc(ClientMethod::Tools(ToolsMethod::List), None),
                rpc(ClientMethod::Tools(ToolsMethod::List), None),
                rpc(ClientMethod::Tools(ToolsMethod::List), None),
            ],
        );
        let event = request_event(batch);
        let p = encode_envelope(&event, "s")["payload"].clone();

        assert_eq!(p["mcp_method"], "tools/list");
        assert_eq!(p["batch_size"], 3);
        assert_eq!(p["session_id"], "sess-b");
    }

    #[test]
    fn encode_request__mcp_batch_empty_uses_blank_method() {
        let batch = Request::McpBatch(req_parts(None), vec![]);
        let event = request_event(batch);
        let p = encode_envelope(&event, "s")["payload"].clone();

        assert_eq!(p["mcp_method"], "");
        assert_eq!(p["batch_size"], 0);
    }

    // ── encode_request: Http ─────────────────────────────────────

    #[test]
    fn encode_request__http_carries_method_path_and_size() {
        let req = HttpReq::builder()
            .method("PUT")
            .uri("/some/path")
            .body(Bytes::copy_from_slice(b"hello"))
            .unwrap();
        let event = request_event(Request::Http(req));
        let p = encode_envelope(&event, "s")["payload"].clone();

        assert_eq!(p["http_method"], "PUT");
        assert_eq!(p["path"], "/some/path");
        assert_eq!(p["request_size"], 5);
    }

    // ── encode_response: Mcp ─────────────────────────────────────

    #[test]
    fn encode_response__mcp_ok_status_and_latency() {
        let resp = Response::Mcp(
            resp_parts(200),
            JsonRpcResult::Response(JsonRpcResponse {
                jsonrpc: JsonRpcVersion,
                id: RequestId::Number(1),
                result: Some(json!({})),
            }),
        );
        let event = response_event(resp, 12_345);
        let p = encode_envelope(&event, "s")["payload"].clone();

        assert_eq!(p["status"], "ok");
        assert_eq!(p["http_status"], 200);
        assert_eq!(p["latency_us"], 12_345);
        assert_eq!(p["error_code"], "");
        assert_eq!(p["error_detail"], "");
    }

    #[test]
    fn encode_response__mcp_error_carries_code_and_message() {
        let resp = Response::Mcp(
            resp_parts(200),
            JsonRpcResult::Error(JsonRpcErrorResponse {
                jsonrpc: JsonRpcVersion,
                id: RequestId::Number(1),
                error: JsonRpcError {
                    code: -32601,
                    message: "method not found".into(),
                    data: None,
                },
            }),
        );
        let event = response_event(resp, 0);
        let p = encode_envelope(&event, "s")["payload"].clone();

        assert_eq!(p["status"], "error");
        assert_eq!(p["error_code"], "-32601");
        assert_eq!(p["error_detail"], "method not found");
    }

    #[test]
    fn encode_response__mcp_request_id_matches_paired_request() {
        let resp = Response::Mcp(
            resp_parts(200),
            JsonRpcResult::Response(JsonRpcResponse {
                jsonrpc: JsonRpcVersion,
                id: RequestId::Number(1),
                result: None,
            }),
        );
        let event = response_event(resp, 1);
        let p = encode_envelope(&event, "s")["payload"].clone();

        assert_eq!(p["request_id"], "req-abc");
    }

    // ── encode_response: McpBatch ────────────────────────────────

    #[test]
    fn encode_response__mcp_batch_all_ok() {
        let resp = Response::McpBatch(
            resp_parts(200),
            vec![
                JsonRpcResult::Response(JsonRpcResponse {
                    jsonrpc: JsonRpcVersion,
                    id: RequestId::Number(1),
                    result: Some(json!({})),
                }),
                JsonRpcResult::Response(JsonRpcResponse {
                    jsonrpc: JsonRpcVersion,
                    id: RequestId::Number(2),
                    result: Some(json!({})),
                }),
            ],
        );
        let event = response_event(resp, 100);
        let p = encode_envelope(&event, "s")["payload"].clone();

        assert_eq!(p["status"], "ok");
        assert_eq!(p["batch_size"], 2);
    }

    #[test]
    fn encode_response__mcp_batch_any_error_marks_status_error() {
        let resp = Response::McpBatch(
            resp_parts(200),
            vec![
                JsonRpcResult::Response(JsonRpcResponse {
                    jsonrpc: JsonRpcVersion,
                    id: RequestId::Number(1),
                    result: Some(json!({})),
                }),
                JsonRpcResult::Error(JsonRpcErrorResponse {
                    jsonrpc: JsonRpcVersion,
                    id: RequestId::Number(2),
                    error: JsonRpcError {
                        code: -32000,
                        message: "boom".into(),
                        data: None,
                    },
                }),
            ],
        );
        let event = response_event(resp, 0);
        let p = encode_envelope(&event, "s")["payload"].clone();

        assert_eq!(p["status"], "error");
    }

    // ── encode_response: Http ────────────────────────────────────

    #[test]
    fn encode_response__http_2xx_marks_status_ok() {
        let http = HttpResp::builder().status(204).body(Bytes::new()).unwrap();
        let event = response_event(Response::Http(http), 50);
        let p = encode_envelope(&event, "s")["payload"].clone();

        assert_eq!(p["http_status"], 204);
        assert_eq!(p["status"], "ok");
    }

    #[test]
    fn encode_response__http_5xx_marks_status_error() {
        let http = HttpResp::builder()
            .status(502)
            .body(Bytes::copy_from_slice(b"upstream is dead"))
            .unwrap();
        let event = response_event(Response::Http(http), 50);
        let p = encode_envelope(&event, "s")["payload"].clone();

        assert_eq!(p["http_status"], 502);
        assert_eq!(p["status"], "error");
        assert_eq!(p["response_size"], 16);
    }

    // ── encode_session ───────────────────────────────────────────

    #[test]
    fn encode_session__active_with_client_info() {
        let mut info = SessionInfo::new(
            "sess-9".into(),
            Some(ClientInfo {
                name: "cursor".into(),
                version: Some("0.42".into()),
            }),
            RequestId::Number(0),
        );
        info.request_count = 5;
        let event = ProxyEvent::Session(Arc::new(info));
        let p = encode_envelope(&event, "s")["payload"].clone();

        assert_eq!(p["session_id"], "sess-9");
        assert_eq!(p["state"], "active");
        assert_eq!(p["client_name"], "cursor");
        assert_eq!(p["client_version"], "0.42");
        assert_eq!(p["request_count"], 5);
    }

    #[test]
    fn encode_session__without_client_info_emits_nulls() {
        let info = SessionInfo::new("sess-x".into(), None, RequestId::Number(0));
        let event = ProxyEvent::Session(Arc::new(info));
        let p = encode_envelope(&event, "s")["payload"].clone();

        assert!(p["client_name"].is_null());
        assert!(p["client_version"].is_null());
        assert!(p["server_name"].is_null());
    }

    #[test]
    fn encode_session__closed_state_serialized() {
        let mut info = SessionInfo::new("sess-c".into(), None, RequestId::Number(0));
        info.state = SessionState::Closed;
        let event = ProxyEvent::Session(Arc::new(info));
        let p = encode_envelope(&event, "s")["payload"].clone();

        assert_eq!(p["state"], "closed");
    }

    // ── encode_schema ────────────────────────────────────────────

    #[test]
    fn encode_schema__tool_carries_name_and_reason() {
        let event = ProxyEvent::Schema(Arc::new(ChangeSchema::Tool(Reason::Added, tool("search"))));
        let env = encode_envelope(&event, "s");

        assert_eq!(env["kind"], "schema");
        assert_eq!(env["payload"]["kind"], "tool");
        assert_eq!(env["payload"]["reason"], "added");
        assert_eq!(env["payload"]["name"], "search");
        assert_eq!(env["payload"]["tool"]["name"], "search");
    }

    #[test]
    fn encode_schema__tool_observed_reason() {
        let event = ProxyEvent::Schema(Arc::new(ChangeSchema::Tool(
            Reason::Observed,
            tool("lookup"),
        )));
        let p = encode_envelope(&event, "s")["payload"].clone();

        assert_eq!(p["reason"], "observed");
    }

    #[test]
    fn encode_schema__prompt() {
        let prompt = Prompt {
            name: "summarize".into(),
            title: None,
            description: None,
            arguments: None,
            meta: None,
        };
        let event = ProxyEvent::Schema(Arc::new(ChangeSchema::Prompt(Reason::Added, prompt)));
        let p = encode_envelope(&event, "s")["payload"].clone();

        assert_eq!(p["kind"], "prompt");
        assert_eq!(p["name"], "summarize");
    }

    #[test]
    fn encode_schema__resource() {
        let resource = Resource {
            uri: "file:///x".into(),
            name: "x".into(),
            title: None,
            description: None,
            mime_type: None,
            size: None,
            annotations: None,
            meta: None,
        };
        let event = ProxyEvent::Schema(Arc::new(ChangeSchema::Resource(Reason::Added, resource)));
        let p = encode_envelope(&event, "s")["payload"].clone();

        assert_eq!(p["kind"], "resource");
        assert_eq!(p["uri"], "file:///x");
    }

    #[test]
    fn encode_schema__resource_template() {
        let rt = ResourceTemplate {
            uri_template: "doc://{id}".into(),
            name: "doc".into(),
            title: None,
            description: None,
            mime_type: None,
            annotations: None,
            meta: None,
        };
        let event = ProxyEvent::Schema(Arc::new(ChangeSchema::ResourceTemplate(Reason::Added, rt)));
        let p = encode_envelope(&event, "s")["payload"].clone();

        assert_eq!(p["kind"], "resource_template");
        assert_eq!(p["uri_template"], "doc://{id}");
    }

    // ── encode_heartbeat ─────────────────────────────────────────

    fn heartbeat_event(
        tunnel_status: &str,
        tunnel_address: Option<&str>,
        upstream: &str,
        export_port: u16,
    ) -> ProxyEvent {
        ProxyEvent::Heartbeat(Arc::new(mcpr_core::event::types::HeartbeatEvent {
            mcp_status: "running".into(),
            tunnel_status: tunnel_status.into(),
            tunnel_address: tunnel_address.map(|s| s.into()),
            upstream: upstream.into(),
            export_port,
            ts: Utc::now(),
        }))
    }

    #[test]
    fn encode_envelope__heartbeat_kind_and_fields() {
        let event = heartbeat_event(
            "connected",
            Some("https://abc.tunnel.mcpr.app"),
            "http://127.0.0.1:8080",
            3004,
        );
        let env = encode_envelope(&event, "prod-server");

        assert_eq!(env["kind"], "heartbeat");
        assert_eq!(env["server"], "prod-server");
        assert_eq!(env["payload"]["mcp_status"], "running");
        assert_eq!(env["payload"]["tunnel_status"], "connected");
        assert_eq!(
            env["payload"]["tunnel_address"],
            "https://abc.tunnel.mcpr.app"
        );
        assert_eq!(env["payload"]["upstream"], "http://127.0.0.1:8080");
        assert_eq!(env["payload"]["export_port"], 3004);
    }

    #[test]
    fn encode_envelope__heartbeat_disabled_tunnel_emits_null_address() {
        let event = heartbeat_event("disabled", None, "http://up:9000", 3000);
        let p = encode_envelope(&event, "s")["payload"].clone();

        assert_eq!(p["tunnel_status"], "disabled");
        assert!(p["tunnel_address"].is_null());
    }

    // ── encode_response timer ────────────────────────────────────

    fn response_event_with_timer(resp: Response, timer: Timer) -> ProxyEvent {
        ProxyEvent::Response(Arc::new(ResponseEvent {
            response: resp,
            request_id: "req-abc".into(),
            latency_us: 0,
            timer,
            ts: Utc::now(),
        }))
    }

    #[test]
    fn encode_response__timer_object_present_with_default_timer() {
        let resp = Response::Mcp(
            resp_parts(200),
            JsonRpcResult::Response(JsonRpcResponse {
                jsonrpc: JsonRpcVersion,
                id: RequestId::Number(1),
                result: Some(json!({})),
            }),
        );
        let event = response_event(resp, 0);
        let p = encode_envelope(&event, "s")["payload"].clone();

        assert!(p["timer"].is_object());
        assert_eq!(p["timer"].as_object().unwrap().len(), 0);
    }

    #[test]
    fn encode_response__timer_object_carries_recorded_spans() {
        let timer = Timer::new();
        let id_a = timer.track_start("Parse");
        timer.track_end(id_a);
        let id_b = timer.track_start("Encode");
        timer.track_end(id_b);

        let resp = Response::Mcp(
            resp_parts(200),
            JsonRpcResult::Response(JsonRpcResponse {
                jsonrpc: JsonRpcVersion,
                id: RequestId::Number(1),
                result: None,
            }),
        );
        let event = response_event_with_timer(resp, timer);
        let p = encode_envelope(&event, "s")["payload"].clone();

        let timer_obj = p["timer"].as_object().expect("timer is object");
        assert!(timer_obj.contains_key("Parse"));
        assert!(timer_obj.contains_key("Encode"));
    }

    #[test]
    fn encode_response__timer_sums_duplicate_span_names() {
        let timer = Timer::new();
        let a = timer.track_start("Stage");
        timer.track_end(a);
        let b = timer.track_start("Stage");
        timer.track_end(b);

        let http = http::Response::builder()
            .status(200)
            .body(Bytes::new())
            .unwrap();
        let event = response_event_with_timer(Response::Http(http), timer);
        let p = encode_envelope(&event, "s")["payload"].clone();

        let timer_obj = p["timer"].as_object().expect("timer is object");
        assert_eq!(timer_obj.len(), 1, "duplicate names collapse to one key");
        assert!(timer_obj["Stage"].is_u64());
    }

    // ── request_id_to_value ──────────────────────────────────────

    #[test]
    fn request_id_to_value__number() {
        assert_eq!(request_id_to_value(&RequestId::Number(42)), json!(42));
    }

    #[test]
    fn request_id_to_value__string() {
        assert_eq!(
            request_id_to_value(&RequestId::String("xyz".into())),
            json!("xyz")
        );
    }

    #[test]
    fn request_id_to_value__null() {
        assert_eq!(request_id_to_value(&RequestId::Null), Value::Null);
    }
}
