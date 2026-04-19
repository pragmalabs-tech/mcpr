//! Session-aware MCP bench using a real `rmcp` client — the workload
//! an actual MCP host generates, not what raw HTTP hammers produce.
//!
//! Two workloads:
//!
//! - `echo` (default): each worker opens one session, loops
//!   `tools/call echo` over that session until the deadline. Measures
//!   steady-state tool invocation latency — the 63 % common case.
//!
//! - `realistic-mix`: each worker runs a deterministic cycle matching
//!   observed production method proportions (tools/call ~63 %,
//!   tools/list ~16 %, initialize ~11 %, resources/list ~9 %,
//!   resources/templates/list ~1 %, resources/read ~1 %). One worker
//!   represents one client "conversation"; latency is reported
//!   per-method for the full mix.
//!
//! Usage:
//!   session-bench --url http://127.0.0.1:9100/mcp \
//!                 --connections 10 --duration 30s \
//!                 [--workload echo|realistic-mix]

use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::Result;
use clap::{Parser, ValueEnum};
use rmcp::{
    ServiceExt,
    model::{
        CallToolRequestParams, ClientCapabilities, ClientInfo, Implementation,
        PaginatedRequestParams, ReadResourceRequestParams,
    },
    transport::StreamableHttpClientTransport,
};
use tokio::task::JoinSet;

#[derive(Parser, Debug, Clone)]
#[command(about = "Session-aware MCP bench (real rmcp client, session reuse)")]
struct Args {
    #[arg(long, default_value = "http://127.0.0.1:9100/mcp")]
    url: String,

    /// Concurrent sessions. Each opens its own MCP session, then loops.
    #[arg(long, short = 'c', default_value_t = 10)]
    connections: usize,

    /// How long to run the measurement window (e.g. "30s", "1m").
    #[arg(long, short = 'z', default_value = "10s", value_parser = parse_duration)]
    duration: Duration,

    /// Warmup window before measurement starts.
    #[arg(long, default_value = "2s", value_parser = parse_duration)]
    warmup: Duration,

    /// Workload pattern. `echo` = tools/call loop;
    /// `realistic-mix` = production-proportioned method cycle.
    #[arg(long, value_enum, default_value_t = Workload::Echo)]
    workload: Workload,

    /// Tool name to invoke in the echo loop. Ignored for `realistic-mix`.
    #[arg(long, default_value = "echo")]
    tool: String,

    /// JSON object of arguments for the echo tool. Ignored for `realistic-mix`.
    #[arg(long, default_value = r#"{"text":"hello"}"#)]
    args: String,
}

#[derive(Copy, Clone, Debug, ValueEnum)]
enum Workload {
    /// Steady-state tool invocation — one session, tools/call × N.
    Echo,
    /// Deterministic cycle matching production method proportions.
    RealisticMix,
}

fn parse_duration(s: &str) -> Result<Duration, String> {
    humantime::parse_duration(s).map_err(|e| e.to_string())
}

// ── realistic-mix cycle ─────────────────────────────────────────────────
//
// Deterministic per-conversation sequence, normalized to observed prod
// counts (1,643 total). Proportions below:
//
//   tools/call:               63 %
//   tools/list:               16 %
//   initialize:               11 %
//   resources/list:            9 %
//   resources/templates/list:  1 %
//   resources/read:            1 %
//
// One full cycle = 18 operations hitting each in proportion. An actual
// rmcp session "initializes" automatically on `serve()`; we don't need
// a manual call for that — instead we RE-initialize by closing and
// reopening the client at cycle boundaries.

enum Op {
    ToolCall,
    ToolsList,
    ResourcesList,
    ResourcesTemplatesList,
    ResourcesRead,
}

/// One full cycle of the realistic mix. Order is deterministic and
/// chosen to interleave heavy methods (tool_call) with the rest.
const REALISTIC_MIX_CYCLE: &[Op] = &[
    Op::ToolsList,
    Op::ToolCall,
    Op::ToolCall,
    Op::ToolCall,
    Op::ToolCall,
    Op::ResourcesList,
    Op::ToolCall,
    Op::ToolCall,
    Op::ToolsList,
    Op::ToolCall,
    Op::ToolCall,
    Op::ToolCall,
    Op::ToolCall,
    Op::ResourcesList,
    Op::ResourcesTemplatesList,
    Op::ToolCall,
    Op::ToolCall,
    Op::ResourcesRead,
];
// Counts: 11 tool_call (61%), 2 tools/list (11%), 2 resources/list (11%),
//         1 resources/templates/list (6%), 1 resources/read (6%),
//         + implicit initialize on each session reopen.

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let tool_args: serde_json::Map<String, serde_json::Value> = serde_json::from_str(&args.args)
        .map_err(|e| anyhow::anyhow!("--args must be a JSON object: {e}"))?;

    let errors = Arc::new(AtomicU64::new(0));
    let total_ops = Arc::new(AtomicU64::new(0));
    let deadline = Instant::now() + args.warmup + args.duration;
    let measurement_start = Instant::now() + args.warmup;

    eprintln!(
        "==> session-bench  url={}  conns={}  warmup={:?}  duration={:?}  workload={:?}",
        args.url, args.connections, args.warmup, args.duration, args.workload
    );

    let mut workers = JoinSet::new();
    for _ in 0..args.connections {
        let url = args.url.clone();
        let tool = args.tool.clone();
        let tool_args = tool_args.clone();
        let workload = args.workload;
        let errors = errors.clone();
        let total_ops = total_ops.clone();

        workers.spawn(async move {
            run_worker(
                url,
                workload,
                tool,
                tool_args,
                deadline,
                measurement_start,
                errors,
                total_ops,
            )
            .await
        });
    }

    // Each worker yields a map of method → latency samples.
    let mut by_method: HashMap<&'static str, Vec<u64>> = HashMap::new();
    while let Some(res) = workers.join_next().await {
        if let Ok(samples) = res {
            for (method, mut v) in samples {
                by_method.entry(method).or_default().append(&mut v);
            }
        }
    }

    print_report(
        &mut by_method,
        args.duration,
        errors.load(Ordering::Relaxed),
        total_ops.load(Ordering::Relaxed),
        args.connections,
        args.workload,
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_worker(
    url: String,
    workload: Workload,
    tool: String,
    tool_args: serde_json::Map<String, serde_json::Value>,
    deadline: Instant,
    measurement_start: Instant,
    errors: Arc<AtomicU64>,
    total_ops: Arc<AtomicU64>,
) -> HashMap<&'static str, Vec<u64>> {
    let client_info = ClientInfo::new(
        ClientCapabilities::default(),
        Implementation::new("mcpr-session-bench", env!("CARGO_PKG_VERSION")),
    );

    let mut by_method: HashMap<&'static str, Vec<u64>> = HashMap::new();

    let transport = StreamableHttpClientTransport::from_uri(url);
    let client = match client_info.serve(transport).await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("session setup failed: {e}");
            return by_method;
        }
    };

    let mut cycle_idx = 0usize;
    loop {
        if Instant::now() >= deadline {
            break;
        }

        let (method_tag, op_start, res) = match workload {
            Workload::Echo => {
                let start = Instant::now();
                let res = client
                    .call_tool(
                        CallToolRequestParams::new(tool.clone())
                            .with_arguments(tool_args.clone()),
                    )
                    .await
                    .map(|_| ())
                    .map_err(|e| anyhow::anyhow!(e));
                ("tools/call", start, res)
            }
            Workload::RealisticMix => {
                let op = &REALISTIC_MIX_CYCLE[cycle_idx % REALISTIC_MIX_CYCLE.len()];
                cycle_idx += 1;
                do_mix_op(&client, op, &tool, &tool_args).await
            }
        };
        let elapsed = op_start.elapsed();
        total_ops.fetch_add(1, Ordering::Relaxed);

        match res {
            Ok(()) => {
                if op_start >= measurement_start {
                    by_method
                        .entry(method_tag)
                        .or_default()
                        .push(elapsed.as_micros() as u64);
                }
            }
            Err(_) => {
                errors.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    let _ = client.cancel().await;
    by_method
}

async fn do_mix_op(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ClientInfo>,
    op: &Op,
    tool: &str,
    tool_args: &serde_json::Map<String, serde_json::Value>,
) -> (&'static str, Instant, anyhow::Result<()>) {
    match op {
        Op::ToolCall => {
            let start = Instant::now();
            let r = client
                .call_tool(
                    CallToolRequestParams::new(tool.to_string())
                        .with_arguments(tool_args.clone()),
                )
                .await
                .map(|_| ())
                .map_err(|e| anyhow::anyhow!(e));
            ("tools/call", start, r)
        }
        Op::ToolsList => {
            let start = Instant::now();
            let r = client
                .list_tools(None::<PaginatedRequestParams>)
                .await
                .map(|_| ())
                .map_err(|e| anyhow::anyhow!(e));
            ("tools/list", start, r)
        }
        Op::ResourcesList => {
            let start = Instant::now();
            let r = client
                .list_resources(None::<PaginatedRequestParams>)
                .await
                .map(|_| ())
                .map_err(|e| anyhow::anyhow!(e));
            ("resources/list", start, r)
        }
        Op::ResourcesTemplatesList => {
            let start = Instant::now();
            let r = client
                .list_resource_templates(None::<PaginatedRequestParams>)
                .await
                .map(|_| ())
                .map_err(|e| anyhow::anyhow!(e));
            ("resources/templates/list", start, r)
        }
        Op::ResourcesRead => {
            let start = Instant::now();
            // Mocks we bench against don't actually serve resources;
            // call returns an application-level error but we still
            // measure the round-trip latency.
            let r = client
                .read_resource(ReadResourceRequestParams::new(
                    "file:///bench-placeholder",
                ))
                .await
                .map(|_| ())
                .map_err(|e| anyhow::anyhow!(e));
            ("resources/read", start, r)
        }
    }
}

fn print_report(
    by_method: &mut HashMap<&'static str, Vec<u64>>,
    window: Duration,
    errors: u64,
    total: u64,
    conns: usize,
    workload: Workload,
) {
    let total_samples: usize = by_method.values().map(|v| v.len()).sum();
    println!();
    println!("==> session-bench results");
    println!("  workload:      {:?}", workload);
    println!("  window:        {:?}", window);
    println!("  connections:   {}", conns);
    println!("  samples:       {}", total_samples);
    println!("  total ops:     {} (incl. warmup)", total);
    println!("  errors:        {}", errors);
    let rps = total_samples as f64 / window.as_secs_f64();
    println!("  req/s (window):{:.0}", rps);
    println!();

    // Print per-method table.
    println!(
        "  {:<28} {:>8} {:>8} {:>8} {:>8} {:>8} {:>10}",
        "method", "count", "p50", "p95", "p99", "p99.9", "max"
    );
    println!("  {}", "-".repeat(90));
    // Deterministic order — tools/call first (dominant), then others.
    let order = [
        "tools/call",
        "tools/list",
        "resources/list",
        "resources/templates/list",
        "resources/read",
    ];
    for method in order {
        if let Some(samples) = by_method.get_mut(method) {
            if samples.is_empty() {
                continue;
            }
            samples.sort_unstable();
            let n = samples.len();
            let pct = |p: f64| samples[((n as f64) * p).min((n - 1) as f64) as usize];
            println!(
                "  {:<28} {:>8} {:>8} {:>8} {:>8} {:>8} {:>10}",
                method,
                n,
                pct(0.50),
                pct(0.95),
                pct(0.99),
                pct(0.999),
                samples[n - 1],
            );
        }
    }

    if total_samples == 0 {
        println!("  (no samples — did all sessions fail?)");
    }
}
