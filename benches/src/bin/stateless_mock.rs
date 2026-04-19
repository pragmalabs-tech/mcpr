//! Stateless MCP upstream for mcpr benchmarking.
//!
//! Hand-rolled JSON-RPC — no session tracking, no protocol ordering.
//! Every request stands alone. Lets load tools (oha/wrk2) hit
//! `tools/call` directly without an initialize handshake, which is
//! how we actually exercise mcpr's steady-state forwarding path.
//!
//! Spec note: omitting `Mcp-Session-Id` on the initialize response
//! signals "server does not support sessions" per Streamable HTTP
//! (2025-03-26). Clients MUST NOT send a session header after that.

use std::{net::SocketAddr, time::Duration};

use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, HeaderValue, StatusCode},
    response::IntoResponse,
    routing::{get, post},
};
use clap::Parser;
use serde_json::{Value, json};

#[derive(Parser, Debug, Clone)]
#[command(about = "Stateless MCP upstream for mcpr benchmarking")]
struct Args {
    #[arg(long, env = "MOCK_BIND", default_value = "127.0.0.1:9001")]
    bind: SocketAddr,

    /// Artificial per-request latency in microseconds. Use to simulate a
    /// realistic upstream (1_000 ≈ in-memory tool, 10_000 ≈ local DB,
    /// 100_000 ≈ HTTP-backed tool). Proxy overhead % collapses toward zero
    /// as this grows — quote absolute µs deltas, not %.
    #[arg(long, env = "MOCK_LATENCY_US", default_value_t = 0)]
    latency_us: u64,
}

#[derive(Clone)]
struct AppState {
    latency: Duration,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    let state = AppState { latency: Duration::from_micros(args.latency_us) };
    let app = Router::new()
        .route("/mcp", post(handle_mcp))
        .route("/healthz", get(|| async { "ok" }))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(args.bind).await.unwrap();
    eprintln!(
        "stateless-mock listening on http://{}/mcp (latency={}µs)",
        args.bind, args.latency_us
    );
    axum::serve(listener, app).await.unwrap();
}

async fn handle_mcp(State(state): State<AppState>, Json(req): Json<Value>) -> impl IntoResponse {
    if !state.latency.is_zero() {
        tokio::time::sleep(state.latency).await;
    }
    let id = req.get("id").cloned().unwrap_or(Value::Null);
    let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");

    let body = match method {
        "initialize" => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "protocolVersion": "2024-11-05",
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "stateless-mock", "version": "0.0.0" }
            }
        }),
        "tools/list" => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "tools": [{
                    "name": "echo",
                    "description": "Echo the given text back",
                    "inputSchema": {
                        "type": "object",
                        "properties": { "text": { "type": "string" } }
                    }
                }]
            }
        }),
        "tools/call" => {
            let text = req
                .pointer("/params/arguments/text")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "content": [{ "type": "text", "text": text }],
                    "isError": false
                }
            })
        }
        "ping" => json!({ "jsonrpc": "2.0", "id": id, "result": {} }),
        _ => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": -32601, "message": "method not found" }
        }),
    };

    // No `mcp-session-id` header — this is the signal for "stateless server"
    // per the Streamable HTTP spec.
    let mut headers = HeaderMap::new();
    headers.insert("content-type", HeaderValue::from_static("application/json"));
    (StatusCode::OK, headers, Json(body))
}
