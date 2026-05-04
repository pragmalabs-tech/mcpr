//! Wire envelope for `ProxyEvent` -> cloud ingest.
//!
//! The cloud ingest endpoint receives an array of these envelopes and
//! lands them verbatim in `events_raw`. ClickHouse materialized views
//! project analytics columns from `payload` on insert, so the wire
//! format stays raw: full JSON-RPC envelopes for MCP traffic. HTTP
//! bodies are NOT shipped (only method/uri/status/sizes) so HTML pages,
//! SSE streams, and probes don't bloat ingest.
//!
//! Each `ProxyEvent::Request` becomes ONE `kind: "request"` envelope
//! carrying both halves of the transaction (`payload.rpc` + `payload.result`,
//! plus latency and session_id). McpBatch flattens to N envelopes - one
//! per rpc, paired with the result whose JSON-RPC id matches; results
//! whose ids don't match any rpc are dropped. Orphans (request stage
//! errored, no upstream call) emit just the request half.
//!
//! ```text
//! { "v": 1, "ts": "...", "server": "<slug>", "kind": "request", "payload": { ... } }
//! ```

use std::collections::HashMap;

use mcpr_core::event::ProxyEvent;
use mcpr_core::event::openai::OpenAiClientContext;
use mcpr_core::event::types::{HeartbeatEvent, LoggedRequest, LoggedResponse, RequestEvent};
use mcpr_core::protocol::mcp::{JsonRpcRequest, JsonRpcResult, RequestId};
use mcpr_core::protocol::schema::{ChangeSchema, Reason};
use mcpr_core::protocol::session::{SessionInfo, SessionState, session_id_from_headers};
use serde_json::{Map, Value, json};

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
    match &re.request {
        LoggedRequest::Mcp(req_parts, rpc) => {
            let req_session = session_id_from_headers(&req_parts.headers);
            let session_id = resolve_session_id(req_session, re.response.as_ref());
            let result = single_result(re.response.as_ref());
            let http_status = mcp_response_status(re.response.as_ref());
            let payload = mcp_payload(
                &re.request_id,
                req_parts.method.as_str(),
                &req_parts.uri.to_string(),
                rpc,
                result,
                http_status,
                &session_id,
                re.latency_us,
                re.upstream_us,
                re.openai.as_ref(),
            );
            vec![envelope("request", ts, server, payload)]
        }
        LoggedRequest::McpBatch(req_parts, rpcs) => {
            let req_session = session_id_from_headers(&req_parts.headers);
            let session_id = resolve_session_id(req_session, re.response.as_ref());
            let by_id = batch_results_by_id(re.response.as_ref());
            let http_status = mcp_response_status(re.response.as_ref());
            let http_method = req_parts.method.as_str();
            let uri = req_parts.uri.to_string();
            rpcs.iter()
                .map(|rpc| {
                    let result = by_id.get(&rpc.id).copied();
                    let payload = mcp_payload(
                        &re.request_id,
                        http_method,
                        &uri,
                        rpc,
                        result,
                        http_status,
                        &session_id,
                        re.latency_us,
                        re.upstream_us,
                        re.openai.as_ref(),
                    );
                    envelope("request", ts.clone(), server, payload)
                })
                .collect()
        }
        LoggedRequest::Http {
            method,
            uri,
            body_size,
        } => {
            let (http_status, response_size) = match re.response.as_ref() {
                Some(LoggedResponse::Http { status, body_size }) => (status.as_u16(), *body_size),
                _ => (0, 0),
            };
            let payload = json!({
                "kind": "http",
                "request_id": re.request_id,
                "session_id": "",
                "http_method": method.as_str(),
                "uri": uri.to_string(),
                "http_status": http_status,
                "request_size": body_size,
                "response_size": response_size,
                "latency_us": re.latency_us,
                "upstream_us": re.upstream_us,
            });
            vec![envelope("request", ts, server, payload)]
        }
    }
}

/// Use the request-header session id when present; otherwise fall back to
/// the response headers. The fallback is required for `initialize`: the
/// request has no session header (server hasn't issued one yet) and the
/// response is where the new id is announced.
fn resolve_session_id(from_request: Option<String>, resp: Option<&LoggedResponse>) -> String {
    if let Some(s) = from_request {
        return s;
    }
    match resp {
        Some(LoggedResponse::Mcp(parts, _)) | Some(LoggedResponse::McpBatch(parts, _)) => {
            session_id_from_headers(&parts.headers).unwrap_or_default()
        }
        _ => String::new(),
    }
}

fn single_result(resp: Option<&LoggedResponse>) -> Option<&JsonRpcResult> {
    match resp {
        Some(LoggedResponse::Mcp(_, result)) => Some(result),
        _ => None,
    }
}

fn batch_results_by_id(resp: Option<&LoggedResponse>) -> HashMap<&RequestId, &JsonRpcResult> {
    let Some(LoggedResponse::McpBatch(_, results)) = resp else {
        return HashMap::new();
    };
    let mut out = HashMap::with_capacity(results.len());
    for r in results {
        let id = match r {
            JsonRpcResult::Response(resp) => &resp.id,
            JsonRpcResult::Error(err) => &err.id,
        };
        out.insert(id, r);
    }
    out
}

fn mcp_response_status(resp: Option<&LoggedResponse>) -> u16 {
    match resp {
        Some(LoggedResponse::Mcp(parts, _)) | Some(LoggedResponse::McpBatch(parts, _)) => {
            parts.status.as_u16()
        }
        _ => 0,
    }
}

#[allow(clippy::too_many_arguments)]
fn mcp_payload(
    request_id: &str,
    http_method: &str,
    uri: &str,
    rpc: &JsonRpcRequest,
    result: Option<&JsonRpcResult>,
    http_status: u16,
    session_id: &str,
    latency_us: u64,
    upstream_us: u64,
    openai: Option<&OpenAiClientContext>,
) -> Value {
    let mut p = Map::new();
    p.insert("kind".into(), json!("mcp"));
    p.insert("request_id".into(), json!(request_id));
    p.insert("session_id".into(), json!(session_id));
    p.insert("http_method".into(), json!(http_method));
    p.insert("uri".into(), json!(uri));
    p.insert("http_status".into(), json!(http_status));
    p.insert(
        "rpc".into(),
        serde_json::to_value(rpc).unwrap_or(Value::Null),
    );
    if let Some(result) = result {
        p.insert(
            "result".into(),
            serde_json::to_value(result).unwrap_or(Value::Null),
        );
    }
    p.insert("latency_us".into(), json!(latency_us));
    p.insert("upstream_us".into(), json!(upstream_us));
    if let Some(openai) = openai {
        p.insert(
            "openai".into(),
            serde_json::to_value(openai).unwrap_or(Value::Null),
        );
    }
    Value::Object(p)
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
            openai: None,
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
            openai: None,
        }))
    }

    fn transaction_with_openai(
        req: LoggedRequest,
        resp: LoggedResponse,
        openai: OpenAiClientContext,
    ) -> ProxyEvent {
        ProxyEvent::Request(Arc::new(RequestEvent {
            request_id: "rid-openai".into(),
            request: req,
            response: Some(resp),
            ts: Utc::now(),
            latency_us: 0,
            upstream_us: 0,
            spans: vec![],
            openai: Some(openai),
        }))
    }

    fn sample_openai() -> OpenAiClientContext {
        OpenAiClientContext {
            session_id: Some("v1/sess".into()),
            subject_id: Some("v1/subj".into()),
            organization_id: Some("v1/org".into()),
            locale: Some("en-US".into()),
            user_agent: Some("chatgpt/test".into()),
            user_location: Some(json!({ "country": "VN", "city": "Vũng Tàu" })),
        }
    }

    // ── envelope shape ───────────────────────────────────────────

    #[test]
    fn encode_envelopes__orphan_emits_one_envelope_without_result() {
        let event = orphan(LoggedRequest::Mcp(
            req_parts(),
            rpc(ClientMethod::Tools(ToolsMethod::List), None),
        ));
        let envs = encode_envelopes(&event, "s");
        assert_eq!(envs.len(), 1);
        assert_eq!(envs[0]["kind"], "request");
        assert_eq!(envs[0]["payload"]["request_id"], "rid-orphan");
        assert!(envs[0]["payload"].get("result").is_none());
    }

    #[test]
    fn encode_envelopes__transaction_emits_one_merged_envelope() {
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
        assert_eq!(envs.len(), 1);
        assert_eq!(envs[0]["kind"], "request");
        assert_eq!(envs[0]["server"], "prod-server");
        assert_eq!(envs[0]["v"], json!(ENVELOPE_VERSION));
        assert_eq!(envs[0]["payload"]["request_id"], "rid-X");
        assert!(envs[0]["payload"]["rpc"].is_object());
        assert!(envs[0]["payload"]["result"].is_object());
    }

    // ── request: Mcp ─────────────────────────────────────────────

    #[test]
    fn encode_envelope__mcp_payload_carries_full_rpc_envelope() {
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
    fn encode_envelope__mcp_session_id_from_request_header() {
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
        assert_eq!(p["session_id"], "sess-1");
    }

    #[test]
    fn encode_envelope__mcp_session_id_falls_back_to_response_header() {
        let resp_with_session = LoggedResponse::Mcp(
            HttpResp::builder()
                .status(200)
                .header("mcp-session-id", "sess-from-resp")
                .body(())
                .unwrap()
                .into_parts()
                .0,
            JsonRpcResult::Response(JsonRpcResponse {
                jsonrpc: JsonRpcVersion,
                id: RequestId::Number(1),
                result: Some(json!({})),
            }),
        );
        let event = transaction(
            LoggedRequest::Mcp(
                req_parts(),
                rpc(ClientMethod::Tools(ToolsMethod::List), None),
            ),
            resp_with_session,
            0,
            0,
        );
        let p = encode_envelopes(&event, "s")[0]["payload"].clone();
        assert_eq!(p["session_id"], "sess-from-resp");
    }

    #[test]
    fn encode_envelope__mcp_session_id_is_empty_when_absent_everywhere() {
        let event = transaction(
            LoggedRequest::Mcp(
                req_parts(),
                rpc(ClientMethod::Tools(ToolsMethod::List), None),
            ),
            ok_response(1),
            0,
            0,
        );
        let p = encode_envelopes(&event, "s")[0]["payload"].clone();
        assert_eq!(p["session_id"], "");
    }

    // ── McpBatch flattening ──────────────────────────────────────

    #[test]
    fn encode_envelope__mcp_batch_flattens_to_one_per_rpc_with_paired_result() {
        let batch = LoggedRequest::McpBatch(
            req_parts(),
            vec![
                {
                    let mut r = rpc(ClientMethod::Tools(ToolsMethod::List), None);
                    r.id = RequestId::Number(1);
                    r
                },
                {
                    let mut r = rpc(ClientMethod::Tools(ToolsMethod::Call), None);
                    r.id = RequestId::Number(2);
                    r
                },
            ],
        );
        let resp = LoggedResponse::McpBatch(
            resp_parts(200),
            vec![
                JsonRpcResult::Response(JsonRpcResponse {
                    jsonrpc: JsonRpcVersion,
                    id: RequestId::Number(2),
                    result: Some(json!({"call_ok": true})),
                }),
                JsonRpcResult::Response(JsonRpcResponse {
                    jsonrpc: JsonRpcVersion,
                    id: RequestId::Number(1),
                    result: Some(json!({"list_ok": true})),
                }),
            ],
        );
        let envs = encode_envelopes(&transaction(batch, resp, 0, 0), "s");

        assert_eq!(envs.len(), 2);
        for env in &envs {
            assert_eq!(env["kind"], "request");
            assert_eq!(env["payload"]["kind"], "mcp");
        }
        assert_eq!(envs[0]["payload"]["rpc"]["method"], "tools/list");
        assert_eq!(envs[0]["payload"]["result"]["result"]["list_ok"], true);
        assert_eq!(envs[1]["payload"]["rpc"]["method"], "tools/call");
        assert_eq!(envs[1]["payload"]["result"]["result"]["call_ok"], true);
    }

    #[test]
    fn encode_envelope__mcp_batch_orphan_emits_no_result_per_rpc() {
        let batch = LoggedRequest::McpBatch(
            req_parts(),
            vec![rpc(ClientMethod::Tools(ToolsMethod::List), None)],
        );
        let envs = encode_envelopes(&orphan(batch), "s");
        assert_eq!(envs.len(), 1);
        assert!(envs[0]["payload"].get("result").is_none());
    }

    // ── openai client context ────────────────────────────────────

    #[test]
    fn encode_envelope__omits_openai_key_when_event_has_none() {
        let event = transaction(
            LoggedRequest::Mcp(
                req_parts(),
                rpc(ClientMethod::Tools(ToolsMethod::Call), None),
            ),
            ok_response(1),
            0,
            0,
        );
        let p = encode_envelopes(&event, "s")[0]["payload"].clone();
        assert!(p.get("openai").is_none());
    }

    #[test]
    fn encode_envelope__includes_openai_block_when_present() {
        let event = transaction_with_openai(
            LoggedRequest::Mcp(
                req_parts(),
                rpc(ClientMethod::Tools(ToolsMethod::Call), None),
            ),
            ok_response(1),
            sample_openai(),
        );
        let p = encode_envelopes(&event, "s")[0]["payload"].clone();

        assert_eq!(p["openai"]["session_id"], "v1/sess");
        assert_eq!(p["openai"]["subject_id"], "v1/subj");
        assert_eq!(p["openai"]["organization_id"], "v1/org");
        assert_eq!(p["openai"]["locale"], "en-US");
        assert_eq!(p["openai"]["user_agent"], "chatgpt/test");
        assert_eq!(p["openai"]["user_location"]["country"], "VN");
    }

    #[test]
    fn encode_envelope__partial_openai_omits_none_fields() {
        let event = transaction_with_openai(
            LoggedRequest::Mcp(
                req_parts(),
                rpc(ClientMethod::Tools(ToolsMethod::Call), None),
            ),
            ok_response(1),
            OpenAiClientContext {
                session_id: Some("v1/only".into()),
                ..Default::default()
            },
        );
        let p = encode_envelopes(&event, "s")[0]["payload"].clone();

        assert_eq!(p["openai"]["session_id"], "v1/only");
        assert!(p["openai"].get("subject_id").is_none());
        assert!(p["openai"].get("user_location").is_none());
    }

    #[test]
    fn encode_envelope__batch_attaches_same_openai_to_each_envelope() {
        let batch_req = LoggedRequest::McpBatch(
            req_parts(),
            vec![
                {
                    let mut r = rpc(ClientMethod::Tools(ToolsMethod::List), None);
                    r.id = RequestId::Number(1);
                    r
                },
                {
                    let mut r = rpc(ClientMethod::Tools(ToolsMethod::Call), None);
                    r.id = RequestId::Number(2);
                    r
                },
            ],
        );
        let batch_resp = LoggedResponse::McpBatch(
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
        let event = transaction_with_openai(batch_req, batch_resp, sample_openai());

        let envs = encode_envelopes(&event, "s");
        assert_eq!(envs.len(), 2);
        assert_eq!(envs[0]["payload"]["openai"]["session_id"], "v1/sess");
        assert_eq!(envs[1]["payload"]["openai"]["session_id"], "v1/sess");
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
    fn encode_envelope__http_carries_method_uri_status_and_sizes() {
        let envs = encode_envelopes(
            &transaction(
                http_req("PUT", "/some/path", b"hello world"),
                LoggedResponse::Http {
                    status: StatusCode::BAD_GATEWAY,
                    body_size: 16,
                },
                200,
                150,
            ),
            "s",
        );
        assert_eq!(envs.len(), 1);
        let p = envs[0]["payload"].clone();

        assert_eq!(p["kind"], "http");
        assert_eq!(p["http_method"], "PUT");
        assert_eq!(p["uri"], "/some/path");
        assert_eq!(p["http_status"], 502);
        assert_eq!(p["request_size"], 11);
        assert_eq!(p["response_size"], 16);
        assert_eq!(p["latency_us"], 200);
        assert_eq!(p["upstream_us"], 150);
        assert!(p.get("body").is_none());
    }

    #[test]
    fn encode_envelope__http_orphan_zero_status_and_response_size() {
        let envs = encode_envelopes(&orphan(http_req("GET", "/", b"")), "s");
        assert_eq!(envs.len(), 1);
        let p = envs[0]["payload"].clone();
        assert_eq!(p["http_status"], 0);
        assert_eq!(p["response_size"], 0);
    }

    // ── result + timing ──────────────────────────────────────────

    #[test]
    fn encode_envelope__mcp_carries_full_result_envelope_and_timing() {
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
        let p = encode_envelopes(&event, "s")[0]["payload"].clone();

        assert_eq!(p["kind"], "mcp");
        assert_eq!(p["http_status"], 200);
        assert_eq!(p["result"]["jsonrpc"], "2.0");
        assert_eq!(p["result"]["id"], 7);
        assert_eq!(p["result"]["result"]["tools"][0]["name"], "search");
        assert_eq!(p["latency_us"], 12_345);
        assert_eq!(p["upstream_us"], 11_200);
    }

    #[test]
    fn encode_envelope__mcp_error_carries_full_error_envelope() {
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
        let p = encode_envelopes(&event, "s")[0]["payload"].clone();

        assert_eq!(p["result"]["error"]["code"], -32601);
        assert_eq!(p["result"]["error"]["message"], "method not found");
    }

    #[test]
    fn encode_envelope__ts_comes_from_event_not_encode_time() {
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
            openai: None,
        }));
        let envs = encode_envelopes(&event, "s");
        assert_eq!(envs[0]["ts"], "2024-01-15T12:30:45.123+00:00");
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
