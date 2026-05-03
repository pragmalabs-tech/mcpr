//! Wire envelope for `ProxyEvent` -> cloud ingest.
//!
//! The cloud ingest endpoint receives an array of these envelopes and
//! lands them verbatim in `events_raw`. ClickHouse materialized views
//! project analytics columns from `payload` on insert, so the wire
//! format stays raw: full JSON-RPC envelopes for MCP traffic. HTTP
//! bodies are NOT shipped (only method/uri/status/body_size) so HTML
//! pages, SSE streams, and probes don't bloat ingest.
//!
//! Each `ProxyEvent::Request` is split on the wire into one "request"
//! envelope and one "response" envelope sharing the same `request_id`
//! (the proxy-minted UUID). McpBatch requests and responses are
//! flattened: one batched event becomes N envelopes, each carrying a
//! single rpc/result. Cloud only ever sees one MCP shape per envelope.
//!
//! ```text
//! { "v": 1, "ts": "...", "server": "<slug>", "kind": "...", "payload": { ... } }
//! ```

use mcpr_core::event::ProxyEvent;
use mcpr_core::event::types::{HeartbeatEvent, LoggedRequest, LoggedResponse, RequestEvent};
use mcpr_core::protocol::mcp::{JsonRpcRequest, JsonRpcResult};
use mcpr_core::protocol::schema::{ChangeSchema, Reason};
use mcpr_core::protocol::session::{SessionInfo, SessionState};
use serde_json::{Value, json};

const ENVELOPE_VERSION: u8 = 1;

/// Encode one `ProxyEvent` into the wire envelopes the cloud expects.
/// Returns `Vec<Value>` because consolidated transactions split into
/// request + response envelopes (and McpBatch variants flatten further);
/// every other variant returns a 1-element vec.
pub fn encode_envelopes(event: &ProxyEvent, server: &str) -> Vec<Value> {
    match event {
        ProxyEvent::Request(re) => encode_transaction_envelopes(re, server),
        ProxyEvent::Session(info) => vec![envelope(
            "session",
            info.last_active.to_rfc3339(),
            server,
            encode_session(info),
        )],
        ProxyEvent::Schema(change) => vec![envelope(
            "schema",
            chrono::Utc::now().to_rfc3339(),
            server,
            encode_schema(change),
        )],
        ProxyEvent::Heartbeat(hb) => vec![envelope(
            "heartbeat",
            hb.ts.to_rfc3339(),
            server,
            encode_heartbeat(hb),
        )],
    }
}

fn envelope(kind: &str, ts: String, server: &str, payload: Value) -> Value {
    json!({
        "v": ENVELOPE_VERSION,
        "ts": ts,
        "server": server,
        "kind": kind,
        "payload": payload,
    })
}

fn encode_transaction_envelopes(re: &RequestEvent, server: &str) -> Vec<Value> {
    let ts = re.ts.to_rfc3339();
    let mut out = encode_request_envelopes(&re.request, server, &ts, &re.request_id);
    if re.response.is_some() {
        out.extend(encode_response_envelopes(re, server, &ts));
    }
    out
}

fn encode_request_envelopes(
    req: &LoggedRequest,
    server: &str,
    ts: &str,
    request_id: &str,
) -> Vec<Value> {
    match req {
        LoggedRequest::Mcp(parts, rpc) => {
            let http_method = parts.method.as_str();
            let uri = parts.uri.to_string();
            vec![envelope(
                "request",
                ts.to_string(),
                server,
                mcp_request_payload(request_id, http_method, &uri, rpc),
            )]
        }
        LoggedRequest::McpBatch(parts, rpcs) => {
            let http_method = parts.method.as_str();
            let uri = parts.uri.to_string();
            rpcs.iter()
                .map(|rpc| {
                    envelope(
                        "request",
                        ts.to_string(),
                        server,
                        mcp_request_payload(request_id, http_method, &uri, rpc),
                    )
                })
                .collect()
        }
        LoggedRequest::Http {
            method,
            uri,
            body_size,
        } => {
            let payload = json!({
                "kind": "http",
                "request_id": request_id,
                "http_method": method.as_str(),
                "uri": uri.to_string(),
                "body_size": body_size,
            });
            vec![envelope("request", ts.to_string(), server, payload)]
        }
    }
}

fn mcp_request_payload(
    request_id: &str,
    http_method: &str,
    uri: &str,
    rpc: &JsonRpcRequest,
) -> Value {
    json!({
        "kind": "mcp",
        "request_id": request_id,
        "http_method": http_method,
        "uri": uri,
        "rpc": rpc,
    })
}

fn encode_response_envelopes(re: &RequestEvent, server: &str, ts: &str) -> Vec<Value> {
    let Some(response) = re.response.as_ref() else {
        return vec![];
    };
    match response {
        LoggedResponse::Mcp(parts, result) => {
            let http_status = parts.status.as_u16();
            vec![envelope(
                "response",
                ts.to_string(),
                server,
                mcp_response_payload(
                    &re.request_id,
                    http_status,
                    result,
                    re.latency_us,
                    re.upstream_us,
                ),
            )]
        }
        LoggedResponse::McpBatch(parts, results) => {
            let http_status = parts.status.as_u16();
            results
                .iter()
                .map(|result| {
                    envelope(
                        "response",
                        ts.to_string(),
                        server,
                        mcp_response_payload(
                            &re.request_id,
                            http_status,
                            result,
                            re.latency_us,
                            re.upstream_us,
                        ),
                    )
                })
                .collect()
        }
        LoggedResponse::Http { status, body_size } => {
            let payload = json!({
                "kind": "http",
                "request_id": re.request_id,
                "http_status": status.as_u16(),
                "body_size": body_size,
                "latency_us": re.latency_us,
                "upstream_us": re.upstream_us,
            });
            vec![envelope("response", ts.to_string(), server, payload)]
        }
    }
}

fn mcp_response_payload(
    request_id: &str,
    http_status: u16,
    result: &JsonRpcResult,
    latency_us: u64,
    upstream_us: u64,
) -> Value {
    json!({
        "kind": "mcp",
        "request_id": request_id,
        "http_status": http_status,
        "result": result,
        "latency_us": latency_us,
        "upstream_us": upstream_us,
    })
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
    use mcpr_core::protocol::schema::{Prompt, Resource, ResourceTemplate, Tool, ToolAnnotations};
    use serde_json::{Map, json};

    // ── Helpers ──────────────────────────────────────────────────

    fn req_parts() -> http::request::Parts {
        HttpReq::builder()
            .method("POST")
            .uri("/")
            .body(())
            .unwrap()
            .into_parts()
            .0
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

    fn ok_response(id: i64) -> LoggedResponse {
        LoggedResponse::Mcp(
            resp_parts(200),
            JsonRpcResult::Response(JsonRpcResponse {
                jsonrpc: JsonRpcVersion,
                id: RequestId::Number(id),
                result: Some(json!({})),
            }),
        )
    }

    fn empty_http_response() -> LoggedResponse {
        LoggedResponse::Http {
            status: StatusCode::OK,
            body_size: 0,
        }
    }

    fn transaction(
        req: LoggedRequest,
        resp: LoggedResponse,
        latency_us: u64,
        upstream_us: u64,
    ) -> ProxyEvent {
        ProxyEvent::Request(Arc::new(RequestEvent {
            request_id: "rid-X".into(),
            request: req,
            response: Some(resp),
            ts: Utc::now(),
            latency_us,
            upstream_us,
            spans: vec![],
        }))
    }

    fn orphan(req: LoggedRequest) -> ProxyEvent {
        ProxyEvent::Request(Arc::new(RequestEvent {
            request_id: "rid-orphan".into(),
            request: req,
            response: None,
            ts: Utc::now(),
            latency_us: 50,
            upstream_us: 0,
            spans: vec![],
        }))
    }

    // ── envelope shape ───────────────────────────────────────────

    #[test]
    fn encode_envelopes__orphan_emits_request_only() {
        let event = orphan(LoggedRequest::Mcp(
            req_parts(),
            rpc(ClientMethod::Tools(ToolsMethod::List), None),
        ));
        let envs = encode_envelopes(&event, "s");
        assert_eq!(envs.len(), 1);
        assert_eq!(envs[0]["kind"], "request");
        assert_eq!(envs[0]["payload"]["request_id"], "rid-orphan");
    }

    #[test]
    fn encode_envelopes__transaction_emits_request_then_response() {
        let event = transaction(
            LoggedRequest::Mcp(
                req_parts(),
                rpc(ClientMethod::Tools(ToolsMethod::List), None),
            ),
            ok_response(1),
            0,
            0,
        );
        let envs = encode_envelopes(&event, "prod-server");
        assert_eq!(envs.len(), 2);
        assert_eq!(envs[0]["kind"], "request");
        assert_eq!(envs[1]["kind"], "response");
        assert_eq!(envs[0]["server"], "prod-server");
        assert_eq!(envs[1]["server"], "prod-server");
        assert_eq!(envs[0]["v"], json!(ENVELOPE_VERSION));
    }

    #[test]
    fn encode_envelopes__request_and_response_share_request_id() {
        let event = transaction(
            LoggedRequest::Mcp(
                req_parts(),
                rpc(ClientMethod::Tools(ToolsMethod::List), None),
            ),
            ok_response(1),
            0,
            0,
        );
        let envs = encode_envelopes(&event, "s");
        assert_eq!(envs[0]["payload"]["request_id"], "rid-X");
        assert_eq!(envs[1]["payload"]["request_id"], "rid-X");
    }

    #[test]
    fn encode_envelopes__request_and_response_share_ts() {
        let event = transaction(
            LoggedRequest::Mcp(
                req_parts(),
                rpc(ClientMethod::Tools(ToolsMethod::List), None),
            ),
            ok_response(1),
            0,
            0,
        );
        let envs = encode_envelopes(&event, "s");
        assert_eq!(envs[0]["ts"], envs[1]["ts"]);
    }

    // ── request: Mcp ─────────────────────────────────────────────

    #[test]
    fn encode_request__mcp_payload_carries_full_rpc_envelope() {
        let mut params = Map::new();
        params.insert("name".into(), json!("search"));
        params.insert("arguments".into(), json!({"q": "rust"}));
        let event = transaction(
            LoggedRequest::Mcp(
                req_parts(),
                rpc(ClientMethod::Tools(ToolsMethod::Call), Some(params)),
            ),
            ok_response(1),
            0,
            0,
        );
        let p = encode_envelopes(&event, "s")[0]["payload"].clone();

        assert_eq!(p["kind"], "mcp");
        assert_eq!(p["http_method"], "POST");
        assert_eq!(p["uri"], "/");
        assert_eq!(p["rpc"]["jsonrpc"], "2.0");
        assert_eq!(p["rpc"]["id"], 1);
        assert_eq!(p["rpc"]["method"], "tools/call");
        assert_eq!(p["rpc"]["params"]["name"], "search");
        assert_eq!(p["rpc"]["params"]["arguments"]["q"], "rust");
    }

    #[test]
    fn encode_request__mcp_no_headers_in_payload() {
        let parts = HttpReq::builder()
            .method("POST")
            .uri("/")
            .header("mcp-session-id", "sess-1")
            .header("authorization", "Bearer secret")
            .body(())
            .unwrap()
            .into_parts()
            .0;
        let event = transaction(
            LoggedRequest::Mcp(parts, rpc(ClientMethod::Tools(ToolsMethod::List), None)),
            ok_response(1),
            0,
            0,
        );
        let p = encode_envelopes(&event, "s")[0]["payload"].clone();

        assert!(p.get("headers").is_none());
        assert!(p.get("session_id").is_none());
    }

    // ── request: McpBatch flattening ─────────────────────────────

    #[test]
    fn encode_request__mcp_batch_flattens_into_n_envelopes() {
        let batch = LoggedRequest::McpBatch(
            req_parts(),
            vec![
                rpc(ClientMethod::Tools(ToolsMethod::List), None),
                rpc(ClientMethod::Tools(ToolsMethod::Call), None),
                rpc(ClientMethod::Tools(ToolsMethod::List), None),
            ],
        );
        let envs = encode_envelopes(&transaction(batch, ok_response(1), 0, 0), "s");

        // 3 request envelopes + 1 response envelope.
        let req_envs: Vec<&Value> = envs.iter().filter(|e| e["kind"] == "request").collect();
        assert_eq!(req_envs.len(), 3);
        for env in &req_envs {
            assert_eq!(env["payload"]["kind"], "mcp");
            assert!(env["payload"]["rpc"].is_object());
            assert!(env["payload"].get("rpcs").is_none());
        }
        assert_eq!(req_envs[0]["payload"]["rpc"]["method"], "tools/list");
        assert_eq!(req_envs[1]["payload"]["rpc"]["method"], "tools/call");
        assert_eq!(req_envs[2]["payload"]["rpc"]["method"], "tools/list");
    }

    // ── request: Http ────────────────────────────────────────────

    fn http_req(method: &str, uri: &str, body: &'static [u8]) -> LoggedRequest {
        LoggedRequest::Http {
            method: Method::from_bytes(method.as_bytes()).unwrap(),
            uri: uri.parse::<Uri>().unwrap(),
            body_size: body.len(),
        }
    }

    #[test]
    fn encode_request__http_carries_method_uri_and_size_only() {
        let envs = encode_envelopes(
            &transaction(
                http_req("PUT", "/some/path", b"hello world"),
                empty_http_response(),
                0,
                0,
            ),
            "s",
        );
        let p = envs[0]["payload"].clone();

        assert_eq!(p["kind"], "http");
        assert_eq!(p["http_method"], "PUT");
        assert_eq!(p["uri"], "/some/path");
        assert_eq!(p["body_size"], 11);
        assert!(p.get("body").is_none());
    }

    // ── response: Mcp with timing ────────────────────────────────

    #[test]
    fn encode_response__mcp_carries_full_result_envelope_and_timing() {
        let resp = LoggedResponse::Mcp(
            resp_parts(200),
            JsonRpcResult::Response(JsonRpcResponse {
                jsonrpc: JsonRpcVersion,
                id: RequestId::Number(7),
                result: Some(json!({"tools": [{"name": "search"}]})),
            }),
        );
        let event = transaction(
            LoggedRequest::Mcp(
                req_parts(),
                rpc(ClientMethod::Tools(ToolsMethod::List), None),
            ),
            resp,
            12_345,
            11_200,
        );
        let envs = encode_envelopes(&event, "s");
        let resp_env = envs.iter().find(|e| e["kind"] == "response").unwrap();
        let p = resp_env["payload"].clone();

        assert_eq!(p["kind"], "mcp");
        assert_eq!(p["http_status"], 200);
        assert_eq!(p["result"]["jsonrpc"], "2.0");
        assert_eq!(p["result"]["id"], 7);
        assert_eq!(p["result"]["result"]["tools"][0]["name"], "search");
        assert_eq!(p["latency_us"], 12_345);
        assert_eq!(p["upstream_us"], 11_200);
    }

    #[test]
    fn encode_response__mcp_error_carries_full_error_envelope() {
        let resp = LoggedResponse::Mcp(
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
        let event = transaction(
            LoggedRequest::Mcp(
                req_parts(),
                rpc(ClientMethod::Tools(ToolsMethod::List), None),
            ),
            resp,
            0,
            0,
        );
        let envs = encode_envelopes(&event, "s");
        let p = envs.iter().find(|e| e["kind"] == "response").unwrap()["payload"].clone();

        assert_eq!(p["result"]["error"]["code"], -32601);
        assert_eq!(p["result"]["error"]["message"], "method not found");
    }

    #[test]
    fn encode_response__ts_comes_from_event_not_encode_time() {
        let frozen_ts = chrono::DateTime::parse_from_rfc3339("2024-01-15T12:30:45.123Z")
            .unwrap()
            .with_timezone(&Utc);
        let event = ProxyEvent::Request(Arc::new(RequestEvent {
            request_id: "rid-Z".into(),
            request: LoggedRequest::Mcp(
                req_parts(),
                rpc(ClientMethod::Tools(ToolsMethod::List), None),
            ),
            response: Some(ok_response(1)),
            ts: frozen_ts,
            latency_us: 0,
            upstream_us: 0,
            spans: vec![],
        }));
        let envs = encode_envelopes(&event, "s");
        for env in &envs {
            assert_eq!(env["ts"], "2024-01-15T12:30:45.123+00:00");
        }
    }

    // ── response: McpBatch flattening ────────────────────────────

    #[test]
    fn encode_response__mcp_batch_flattens_with_shared_timing() {
        let resp = LoggedResponse::McpBatch(
            resp_parts(200),
            vec![
                JsonRpcResult::Response(JsonRpcResponse {
                    jsonrpc: JsonRpcVersion,
                    id: RequestId::Number(1),
                    result: Some(json!({"ok": 1})),
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
        let event = transaction(
            LoggedRequest::Mcp(
                req_parts(),
                rpc(ClientMethod::Tools(ToolsMethod::List), None),
            ),
            resp,
            5_000,
            4_800,
        );
        let envs = encode_envelopes(&event, "s");

        let resp_envs: Vec<&Value> = envs.iter().filter(|e| e["kind"] == "response").collect();
        assert_eq!(resp_envs.len(), 2);
        for env in &resp_envs {
            assert_eq!(env["payload"]["kind"], "mcp");
            assert_eq!(env["payload"]["latency_us"], 5_000);
            assert_eq!(env["payload"]["upstream_us"], 4_800);
            assert!(env["payload"].get("results").is_none());
        }
        assert_eq!(resp_envs[0]["payload"]["result"]["result"]["ok"], 1);
        assert_eq!(resp_envs[1]["payload"]["result"]["error"]["code"], -32000);
    }

    // ── response: Http ───────────────────────────────────────────

    #[test]
    fn encode_response__http_carries_status_body_size_and_timing() {
        let resp = LoggedResponse::Http {
            status: StatusCode::BAD_GATEWAY,
            body_size: 16,
        };
        let event = transaction(http_req("GET", "/", b""), resp, 200, 150);
        let envs = encode_envelopes(&event, "s");
        let p = envs.iter().find(|e| e["kind"] == "response").unwrap()["payload"].clone();

        assert_eq!(p["kind"], "http");
        assert_eq!(p["http_status"], 502);
        assert_eq!(p["body_size"], 16);
        assert!(p.get("body").is_none());
        assert_eq!(p["latency_us"], 200);
        assert_eq!(p["upstream_us"], 150);
    }

    // ── session ──────────────────────────────────────────────────

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
        let p = encode_envelopes(&ProxyEvent::Session(Arc::new(info)), "s").remove(0)["payload"]
            .clone();

        assert_eq!(p["session_id"], "sess-9");
        assert_eq!(p["state"], "active");
        assert_eq!(p["client_name"], "cursor");
        assert_eq!(p["client_version"], "0.42");
        assert_eq!(p["request_count"], 5);
    }

    // ── schema ───────────────────────────────────────────────────

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

    #[test]
    fn encode_schema__tool_carries_full_definition() {
        let event = ProxyEvent::Schema(Arc::new(ChangeSchema::Tool(Reason::Added, tool("search"))));
        let p = encode_envelopes(&event, "s").remove(0)["payload"].clone();

        assert_eq!(p["kind"], "tool");
        assert_eq!(p["reason"], "added");
        assert_eq!(p["name"], "search");
        assert_eq!(p["tool"]["name"], "search");
    }

    #[test]
    fn encode_schema__prompt_resource_template_kinds() {
        let prompt = Prompt {
            name: "summarize".into(),
            title: None,
            description: None,
            arguments: None,
            meta: None,
        };
        let p = encode_envelopes(
            &ProxyEvent::Schema(Arc::new(ChangeSchema::Prompt(Reason::Added, prompt))),
            "s",
        )
        .remove(0)["payload"]
            .clone();
        assert_eq!(p["kind"], "prompt");
        assert_eq!(p["name"], "summarize");

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
        let p = encode_envelopes(
            &ProxyEvent::Schema(Arc::new(ChangeSchema::Resource(Reason::Added, resource))),
            "s",
        )
        .remove(0)["payload"]
            .clone();
        assert_eq!(p["kind"], "resource");
        assert_eq!(p["uri"], "file:///x");

        let rt = ResourceTemplate {
            uri_template: "doc://{id}".into(),
            name: "doc".into(),
            title: None,
            description: None,
            mime_type: None,
            annotations: None,
            meta: None,
        };
        let p = encode_envelopes(
            &ProxyEvent::Schema(Arc::new(ChangeSchema::ResourceTemplate(Reason::Added, rt))),
            "s",
        )
        .remove(0)["payload"]
            .clone();
        assert_eq!(p["kind"], "resource_template");
        assert_eq!(p["uri_template"], "doc://{id}");
    }

    // ── heartbeat ────────────────────────────────────────────────

    #[test]
    fn encode_heartbeat__carries_status_and_address() {
        let hb = HeartbeatEvent {
            mcp_status: "running".into(),
            tunnel_status: "connected".into(),
            tunnel_address: Some("https://abc.tunnel.mcpr.app".into()),
            upstream: "http://127.0.0.1:8080".into(),
            export_port: 3004,
            ts: Utc::now(),
        };
        let env = encode_envelopes(&ProxyEvent::Heartbeat(Arc::new(hb)), "prod-server").remove(0);

        assert_eq!(env["kind"], "heartbeat");
        assert_eq!(env["server"], "prod-server");
        assert_eq!(env["payload"]["mcp_status"], "running");
        assert_eq!(env["payload"]["tunnel_status"], "connected");
        assert_eq!(
            env["payload"]["tunnel_address"],
            "https://abc.tunnel.mcpr.app"
        );
        assert_eq!(env["payload"]["export_port"], 3004);
    }
}
