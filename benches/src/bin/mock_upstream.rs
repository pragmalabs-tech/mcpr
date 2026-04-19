//! Minimal MCP upstream built on `rmcp` for mcpr benchmarking.
//!
//! Serves Streamable HTTP at /mcp with one tool (`echo`). Real protocol,
//! real session handling — so proxy overhead numbers reflect actual MCP
//! traffic shapes, not a hand-rolled JSON-RPC stub.

use std::net::SocketAddr;

use clap::Parser;
use rmcp::{
    ErrorData as McpError, ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, Content, Implementation, ProtocolVersion, ServerCapabilities, ServerInfo},
    tool, tool_handler, tool_router,
    transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
    },
};

#[derive(Parser, Debug, Clone)]
#[command(about = "Mock MCP upstream for mcpr benchmarking (rmcp streamable-http)")]
struct Args {
    #[arg(long, env = "MOCK_BIND", default_value = "127.0.0.1:9001")]
    bind: SocketAddr,
}

#[derive(Clone)]
struct Echo {
    #[allow(dead_code)]
    tool_router: ToolRouter<Echo>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct EchoArgs {
    #[serde(default)]
    text: String,
}

#[tool_router]
impl Echo {
    fn new() -> Self {
        Self { tool_router: Self::tool_router() }
    }

    #[tool(description = "Echo the given text back to the caller")]
    async fn echo(
        &self,
        Parameters(args): Parameters<EchoArgs>,
    ) -> Result<CallToolResult, McpError> {
        Ok(CallToolResult::success(vec![Content::text(args.text)]))
    }
}

#[tool_handler]
impl ServerHandler for Echo {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder().enable_tools().build(),
        )
        .with_server_info(Implementation::from_build_env())
        .with_protocol_version(ProtocolVersion::V_2024_11_05)
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let ct = tokio_util::sync::CancellationToken::new();

    let service = StreamableHttpService::new(
        || Ok(Echo::new()),
        LocalSessionManager::default().into(),
        StreamableHttpServerConfig::default().with_cancellation_token(ct.child_token()),
    );

    let router = axum::Router::new()
        .route("/healthz", axum::routing::get(|| async { "ok" }))
        .nest_service("/mcp", service);

    let listener = tokio::net::TcpListener::bind(args.bind).await?;
    eprintln!("mock-upstream listening on http://{}/mcp", args.bind);

    let shutdown = ct.clone();
    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            let _ = tokio::signal::ctrl_c().await;
            shutdown.cancel();
        })
        .await?;
    Ok(())
}
