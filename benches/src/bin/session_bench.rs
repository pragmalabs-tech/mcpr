//! Session-reuse bench — what `oha` can't do.
//!
//! Each worker opens one real rmcp MCP session (initialize → server returns
//! `Mcp-Session-Id`), then loops `tools/call echo` over that same session
//! until the deadline. This is the realistic MCP usage pattern — one long
//! conversation with many tool invocations — and tests mcpr's steady-state
//! forwarding path with session attachment (not session churn).
//!
//! Usage:
//!   session-bench --url http://127.0.0.1:9100/mcp \
//!                 --connections 100 --duration 30s [--tool echo]

use std::{
    sync::{Arc, atomic::{AtomicU64, Ordering}},
    time::{Duration, Instant},
};

use anyhow::Result;
use clap::Parser;
use rmcp::{
    ServiceExt,
    model::{CallToolRequestParams, ClientCapabilities, ClientInfo, Implementation},
    transport::StreamableHttpClientTransport,
};
use tokio::task::JoinSet;

#[derive(Parser, Debug, Clone)]
#[command(about = "Session-reuse MCP bench (real rmcp client, steady-state tools/call loop)")]
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

    /// Tool name to invoke in the loop.
    #[arg(long, default_value = "echo")]
    tool: String,

    /// JSON object of arguments to pass to the tool each call.
    #[arg(long, default_value = r#"{"text":"hello"}"#)]
    args: String,
}

fn parse_duration(s: &str) -> Result<Duration, String> {
    humantime::parse_duration(s).map_err(|e| e.to_string())
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let tool_args: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(&args.args).map_err(|e| anyhow::anyhow!("--args must be a JSON object: {e}"))?;

    let errors = Arc::new(AtomicU64::new(0));
    let total_calls = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(tokio::sync::Notify::new());

    let deadline = Instant::now() + args.warmup + args.duration;
    let measurement_start = Instant::now() + args.warmup;

    eprintln!(
        "==> session-bench  url={}  conns={}  warmup={:?}  duration={:?}  tool={}",
        args.url, args.connections, args.warmup, args.duration, args.tool
    );

    let mut workers = JoinSet::new();
    for _ in 0..args.connections {
        let url = args.url.clone();
        let tool = args.tool.clone();
        let tool_args = tool_args.clone();
        let errors = errors.clone();
        let total_calls = total_calls.clone();
        let stop = stop.clone();

        workers.spawn(async move {
            let transport = StreamableHttpClientTransport::from_uri(url);
            let client_info = ClientInfo::new(
                ClientCapabilities::default(),
                Implementation::new("mcpr-session-bench", env!("CARGO_PKG_VERSION")),
            );
            let client = match client_info.serve(transport).await {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("session setup failed: {e}");
                    return Vec::<u64>::new();
                }
            };

            let mut latencies = Vec::with_capacity(4096);
            loop {
                if Instant::now() >= deadline {
                    break;
                }
                let start = Instant::now();
                let res = client
                    .call_tool(
                        CallToolRequestParams::new(tool.clone())
                            .with_arguments(tool_args.clone()),
                    )
                    .await;
                let elapsed = start.elapsed();
                match res {
                    Ok(_) => {
                        total_calls.fetch_add(1, Ordering::Relaxed);
                        // Only record measurement-window samples.
                        if start >= measurement_start {
                            latencies.push(elapsed.as_micros() as u64);
                        }
                    }
                    Err(_) => {
                        errors.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            let _ = client.cancel().await;
            let _ = stop;
            latencies
        });
    }

    // Collect.
    let mut all = Vec::<u64>::new();
    while let Some(res) = workers.join_next().await {
        if let Ok(v) = res {
            all.extend(v);
        }
    }

    print_report(&mut all, args.duration, errors.load(Ordering::Relaxed), total_calls.load(Ordering::Relaxed), args.connections);
    Ok(())
}

fn print_report(samples: &mut [u64], window: Duration, errors: u64, total: u64, conns: usize) {
    if samples.is_empty() {
        println!("(no samples — did all sessions fail?)  errors={errors}  total={total}");
        return;
    }
    samples.sort_unstable();
    let n = samples.len();
    let pct = |p: f64| -> u64 {
        let idx = ((n as f64) * p).min((n - 1) as f64) as usize;
        samples[idx]
    };
    let rps = samples.len() as f64 / window.as_secs_f64();
    let mean = samples.iter().sum::<u64>() as f64 / n as f64;

    println!();
    println!("==> session-bench results");
    println!("  window:        {:?}", window);
    println!("  connections:   {}", conns);
    println!("  samples:       {}", n);
    println!("  total calls:   {} (incl. warmup)", total);
    println!("  errors:        {}", errors);
    println!("  req/s (window):{:.0}", rps);
    println!();
    println!("  latency (µs) — p50 / p95 / p99 / p99.9 / max / mean");
    println!(
        "  {} / {} / {} / {} / {} / {:.0}",
        pct(0.50),
        pct(0.95),
        pct(0.99),
        pct(0.999),
        samples[n - 1],
        mean
    );
}
