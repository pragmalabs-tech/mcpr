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
use mcpr_core::protocol::mcp::{JsonRpcRequest, JsonRpcResult, RequestId};
use mcpr_core::protocol::schema::{ChangeSchema, Reason};
use mcpr_core::protocol::session::{SessionInfo, SessionState, session_id_from_headers};
use mcpr_core::protocol::{Request, Response};
use serde_json::{Value, json};

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
            encode_response(&re.response, &re.request_id, re.latency_us),
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

fn encode_response(resp: &Response, request_id: &str, latency_us: u64) -> Value {
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
            })
        }
    }
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

fn request_id_to_value(id: &RequestId) -> Value {
    match id {
        RequestId::Number(n) => json!(*n),
        RequestId::String(s) => json!(s),
        RequestId::Null => Value::Null,
    }
}
