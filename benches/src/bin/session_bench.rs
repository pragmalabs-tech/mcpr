//! Minimal MCP load driver, hand-rolled HTTP.
//!
//! Each worker:
//!   1. POST /mcp with `initialize`, captures `mcp-session-id` header.
//!   2. POST /mcp with `notifications/initialized` (best-effort).
//!   3. Loop POST /mcp with `tools/call <tool>` until the deadline.
//!
//! Reports count, p50, p95, p99, p99.9, and max latency in microseconds.
//!
//! Usage:
//!   session-bench --url http://127.0.0.1:9100/mcp \
//!                 --tool get_weather \
//!                 --args '{"city":"Tokyo"}' \
//!                 -c 1 -z 10s --warmup 2s

use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow};
use clap::Parser;
use reqwest::Client;
use serde_json::{Value, json};
use tokio::task::JoinSet;

#[derive(Parser, Debug, Clone)]
#[command(about = "Hand-rolled MCP bench (raw HTTP, no rmcp client)")]
struct Args {
    #[arg(long, default_value = "http://127.0.0.1:9100/mcp")]
    url: String,

    /// Concurrent sessions. Each opens its own MCP session, then loops.
    #[arg(long, short = 'c', default_value_t = 1)]
    connections: usize,

    /// Measurement window (e.g. "10s", "1m").
    #[arg(long, short = 'z', default_value = "10s", value_parser = parse_duration)]
    duration: Duration,

    /// Warmup window before measurement starts.
    #[arg(long, default_value = "2s", value_parser = parse_duration)]
    warmup: Duration,

    /// Tool name to invoke each iteration.
    #[arg(long, default_value = "get_weather")]
    tool: String,

    /// JSON object of arguments for the tool.
    #[arg(long, default_value = r#"{"city":"Tokyo"}"#)]
    args: String,

    /// Optional label printed in the report header.
    #[arg(long)]
    label: Option<String>,
}

fn parse_duration(s: &str) -> Result<Duration, String> {
    humantime::parse_duration(s).map_err(|e| e.to_string())
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let tool_args: Value =
        serde_json::from_str(&args.args).context("--args must be a valid JSON object")?;

    let errors = Arc::new(AtomicU64::new(0));
    let total_ops = Arc::new(AtomicU64::new(0));
    let deadline = Instant::now() + args.warmup + args.duration;
    let measurement_start = Instant::now() + args.warmup;

    eprintln!(
        "==> session-bench  url={}  conns={}  warmup={:?}  duration={:?}  tool={}  args={}",
        args.url, args.connections, args.warmup, args.duration, args.tool, args.args
    );

    let mut workers = JoinSet::new();
    for _ in 0..args.connections {
        let url = args.url.clone();
        let tool = args.tool.clone();
        let tool_args = tool_args.clone();
        let errors = errors.clone();
        let total_ops = total_ops.clone();

        workers.spawn(async move {
            run_worker(
                url,
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

    let mut samples: Vec<u64> = Vec::new();
    while let Some(res) = workers.join_next().await {
        if let Ok(mut v) = res {
            samples.append(&mut v);
        }
    }

    print_report(
        args.label.as_deref(),
        &args.tool,
        &mut samples,
        args.duration,
        errors.load(Ordering::Relaxed),
        total_ops.load(Ordering::Relaxed),
        args.connections,
    );
    Ok(())
}

async fn run_worker(
    url: String,
    tool: String,
    tool_args: Value,
    deadline: Instant,
    measurement_start: Instant,
    errors: Arc<AtomicU64>,
    total_ops: Arc<AtomicU64>,
) -> Vec<u64> {
    let mut samples: Vec<u64> = Vec::with_capacity(50_000);

    let client = match Client::builder()
        .pool_max_idle_per_host(2)
        .timeout(Duration::from_secs(10))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("client build failed: {e}");
            return samples;
        }
    };

    let session_id = match initialize_session(&client, &url).await {
        Ok(id) => id,
        Err(e) => {
            eprintln!("initialize failed: {e}");
            return samples;
        }
    };

    // Best-effort initialized notification. Server returns 202; we don't
    // care about the body. Failure here is non-fatal.
    let _ = client
        .post(&url)
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .header("mcp-session-id", &session_id)
        .json(&json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        }))
        .send()
        .await;

    let call_template = json!({
        "jsonrpc": "2.0",
        "id": 0,
        "method": "tools/call",
        "params": {
            "name": tool,
            "arguments": tool_args,
        }
    });

    let mut req_id: u64 = 1;
    loop {
        if Instant::now() >= deadline {
            break;
        }
        req_id += 1;
        let mut body = call_template.clone();
        body["id"] = json!(req_id);

        let start = Instant::now();
        let res = client
            .post(&url)
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .header("mcp-session-id", &session_id)
            .json(&body)
            .send()
            .await;

        let outcome = match res {
            Ok(r) if r.status().is_success() => match r.bytes().await {
                Ok(_) => Ok(start.elapsed()),
                Err(e) => Err(anyhow!("body read: {e}")),
            },
            Ok(r) => Err(anyhow!("http status {}", r.status())),
            Err(e) => Err(anyhow!("send: {e}")),
        };
        total_ops.fetch_add(1, Ordering::Relaxed);
        match outcome {
            Ok(elapsed) => {
                if start >= measurement_start {
                    samples.push(elapsed.as_micros() as u64);
                }
            }
            Err(_) => {
                errors.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    samples
}

async fn initialize_session(client: &Client, url: &str) -> Result<String> {
    let body = json!({
        "jsonrpc": "2.0",
        "id": 0,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-03-26",
            "capabilities": {},
            "clientInfo": {
                "name": "mcpr-session-bench",
                "version": env!("CARGO_PKG_VERSION"),
            }
        }
    });
    let resp = client
        .post(url)
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .json(&body)
        .send()
        .await
        .context("initialize POST")?;
    if !resp.status().is_success() {
        return Err(anyhow!("initialize returned http {}", resp.status()));
    }
    let session_id = resp
        .headers()
        .get("mcp-session-id")
        .ok_or_else(|| anyhow!("upstream did not return mcp-session-id"))?
        .to_str()
        .context("mcp-session-id is not ASCII")?
        .to_string();
    let _ = resp.bytes().await;
    Ok(session_id)
}

fn print_report(
    label: Option<&str>,
    tool: &str,
    samples: &mut Vec<u64>,
    window: Duration,
    errors: u64,
    total: u64,
    conns: usize,
) {
    let n = samples.len();
    println!();
    println!("==> session-bench results");
    if let Some(label) = label {
        println!("  label:         {}", label);
    }
    println!("  tool:          {}", tool);
    println!("  window:        {:?}", window);
    println!("  connections:   {}", conns);
    println!("  samples:       {}", n);
    println!("  total ops:     {} (incl. warmup)", total);
    println!("  errors:        {}", errors);
    let rps = n as f64 / window.as_secs_f64();
    println!("  req/s (window):{:.0}", rps);
    println!();

    println!(
        "  {:>8} {:>8} {:>8} {:>8} {:>10}",
        "p50", "p95", "p99", "p99.9", "max"
    );
    println!("  {}", "-".repeat(48));
    if n == 0 {
        println!("  (no samples)");
        return;
    }
    samples.sort_unstable();
    let pct = |p: f64| samples[((n as f64) * p).min((n - 1) as f64) as usize];
    println!(
        "  {:>8} {:>8} {:>8} {:>8} {:>10}",
        pct(0.50),
        pct(0.95),
        pct(0.99),
        pct(0.999),
        samples[n - 1],
    );
    println!();
    println!("  (latencies in microseconds)");
}
