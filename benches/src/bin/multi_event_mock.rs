//! Multi-event SSE mock — emits a deterministic 2-event SSE response
//! to POST /mcp, with full SSE framing (empty data prefix, `id:`,
//! `retry:`, blank-line terminators). Used by
//! `scripts/scenarios/multi-event-sse.sh` to verify mcpr forwards
//! multi-event streams byte-for-byte.

use std::net::SocketAddr;

use axum::{
    Router,
    http::{HeaderMap, HeaderValue, StatusCode},
    response::IntoResponse,
    routing::{get, post},
};
use clap::Parser;

#[derive(Parser, Debug, Clone)]
#[command(about = "Multi-event SSE mock for mcpr benchmarking")]
struct Args {
    #[arg(long, env = "MOCK_BIND", default_value = "127.0.0.1:9001")]
    bind: SocketAddr,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    let app = Router::new()
        .route("/mcp", post(handle_mcp))
        .route("/healthz", get(|| async { "ok" }));

    let listener = tokio::net::TcpListener::bind(args.bind).await.unwrap();
    eprintln!("multi-event-mock listening on http://{}/mcp", args.bind);
    axum::serve(listener, app).await.unwrap();
}

/// Deterministic SSE body. Two events + SSE metadata lines.
/// Whatever mcpr forwards back should match these bytes verbatim.
const MULTI_EVENT_BODY: &[u8] = b"data: \n\
id: 0\n\
retry: 3000\n\
\n\
data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\",\"params\":{\"progress\":50}}\n\
\n\
data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}\n\
\n";

async fn handle_mcp() -> impl IntoResponse {
    let mut headers = HeaderMap::new();
    headers.insert(
        "content-type",
        HeaderValue::from_static("text/event-stream"),
    );
    headers.insert("cache-control", HeaderValue::from_static("no-cache"));
    headers.insert(
        "mcp-session-id",
        HeaderValue::from_static("multi-event-session"),
    );

    (StatusCode::OK, headers, MULTI_EVENT_BODY)
}
