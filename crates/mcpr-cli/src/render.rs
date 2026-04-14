//! Terminal rendering layer.
//!
//! Every function in this module receives **data** and writes it to
//! stdout/stderr.  No database access, no process management — just
//! formatting and printing.  Commands call into this module after
//! obtaining data from `logic::*`.

use std::path::Path;

use mcpr_core::proxy::state::SharedProxyState;
use mcpr_integrations::store::query::{
    clients::ClientRow,
    logs::LogRow,
    schema::{SchemaChangeRow, SchemaRow, SchemaStatusRow, SchemaToolUsageRow},
    session_detail::SessionDetail,
    sessions::SessionRow,
    stats::StatsResult,
    store_ops::{StoreStats, VacuumResult},
};

use crate::proxy_lock::{LockInfo, LockStatus};

// ── Output mode ───────────────────────────────────────────────────────

/// Whether to render human-readable tables or machine-readable JSON.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum OutputMode {
    Pretty,
    Json,
}

impl From<bool> for OutputMode {
    /// Convert a `--json` flag into an `OutputMode`.
    fn from(json: bool) -> Self {
        if json {
            OutputMode::Json
        } else {
            OutputMode::Pretty
        }
    }
}

// ── Format helpers ────────────────────────────────────────────────────

/// Display-friendly proxy name: the name itself or "all proxies".
pub fn proxy_display(name: &Option<String>) -> &str {
    name.as_deref().unwrap_or("all proxies")
}

/// Format a unix ms timestamp as a human-readable local time.
pub fn format_ts(ts: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ts)
        .map(|dt| {
            dt.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(|| "?".to_string())
}

/// Format bytes as a human-readable size.
pub fn format_bytes(bytes: u64) -> String {
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.1} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    } else if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

/// Format an optional byte count for table columns, showing "—" for None/zero.
pub fn format_bytes_col(bytes: Option<i64>) -> String {
    match bytes {
        Some(b) if b > 0 => format_bytes(b as u64),
        _ => "—".to_string(),
    }
}

/// Format latency (in μs) for human-readable display.
pub fn format_latency(us: i64) -> String {
    mcpr_core::time::format_latency_us(us)
}

/// Print a serializable struct as a single JSON line.
pub fn print_json(value: &impl serde::Serialize) {
    if let Ok(json) = serde_json::to_string(value) {
        println!("{json}");
    }
}

// ── Startup banner ────────────────────────────────────────────────────

/// Populate the proxy state with startup info and print a startup banner to stderr.
pub fn log_startup(
    state: &SharedProxyState,
    port: u16,
    public_url: &str,
    mcp_upstream: &str,
    widgets: Option<&str>,
) {
    let mut s = mcpr_core::proxy::lock_state(state);
    s.proxy_url = format!("http://localhost:{port}");
    s.tunnel_url = public_url.to_string();
    s.mcp_upstream = mcp_upstream.to_string();
    s.widgets = widgets.unwrap_or("(none)").to_string();
    drop(s);

    eprintln!();
    eprintln!("  {} mcpr proxy running", colored::Colorize::green("ready"),);
    eprintln!("  proxy:    http://localhost:{port}");
    if public_url != format!("http://localhost:{port}") {
        eprintln!("  tunnel:   {public_url}");
    }
    eprintln!("  upstream: {mcp_upstream}");
    if let Some(w) = widgets {
        eprintln!("  widgets:  {w}");
    }
    eprintln!();
}

// ── Version / Validate ────────────────────────────────────────────────

/// Print version information as JSON.
pub fn version_info() {
    println!(
        "{}",
        serde_json::json!({
            "version": env!("CARGO_PKG_VERSION"),
            "target": option_env!("TARGET").unwrap_or("unknown"),
        })
    );
}

/// Print config validation results with colored severity.
pub fn validate_issues(issues: &[(&str, String)]) {
    for (severity, msg) in issues {
        match *severity {
            "error" => {
                eprintln!("  {} {msg}", colored::Colorize::red("error"));
            }
            "warn" => {
                eprintln!("  {} {msg}", colored::Colorize::yellow("warn"));
            }
            _ => {
                eprintln!("  {} {msg}", colored::Colorize::green("ok"));
            }
        }
    }
}

// ── Daemon status ─────────────────────────────────────────────────────

/// Computed daemon status info for rendering.
pub struct DaemonStatusInfo {
    pub pid: u32,
    pub hours: i64,
    pub minutes: i64,
    pub seconds: i64,
    pub pid_file: std::path::PathBuf,
    pub log_file: std::path::PathBuf,
    pub running_proxies: Vec<(String, LockInfo)>,
}

/// Daemon is not running or has a stale PID file.
pub enum DaemonStatusError {
    Stale { pid: u32 },
    NotRunning,
}

/// Render daemon status to stdout/stderr.  Returns the process exit code.
pub fn daemon_status(result: Result<DaemonStatusInfo, DaemonStatusError>) -> i32 {
    match result {
        Ok(info) => {
            println!(
                "mcprd running (PID: {}, uptime: {}h {}m {}s)",
                info.pid, info.hours, info.minutes, info.seconds
            );
            println!("  PID file: {}", info.pid_file.display());
            println!("  Log file: {}", info.log_file.display());

            if info.running_proxies.is_empty() {
                println!("\n  0 proxies running.");
                println!("  Use `mcpr proxy run <config>` to start a proxy.");
            } else {
                println!("\n  {} proxy(ies) running:", info.running_proxies.len());
                for (name, lock) in &info.running_proxies {
                    println!("    {} (PID: {}, port: {})", name, lock.pid, lock.port);
                }
                println!();
                println!("  Use `mcpr proxy logs` to view request logs.");
                println!("  Use `mcpr proxy stats` to view metrics.");
            }
            0
        }
        Err(DaemonStatusError::Stale { pid }) => {
            eprintln!("Daemon is not running (stale PID file for PID: {})", pid);
            1
        }
        Err(DaemonStatusError::NotRunning) => {
            eprintln!("No daemon is running.");
            1
        }
    }
}

// ── Proxy lifecycle ───────────────────────────────────────────────────

pub fn proxy_started(name: &str) {
    eprintln!("Start proxy \"{}\".", name);
}

pub fn proxy_restarted(name: &str) {
    eprintln!("Restarted proxy \"{}\".", name);
}

pub fn proxy_stopping(name: &str, pid: u32) {
    eprintln!("Stopping proxy \"{}\" (pid {})...", name, pid);
}

pub fn proxy_stopped_done() {
    eprintln!("Stopped.");
}

pub fn proxy_stale_cleaned(name: &str) {
    eprintln!("Cleaned up stale lock for proxy \"{}\".", name);
}

pub fn no_running_proxies() {
    eprintln!("No running proxies found.");
}

pub fn stopped_proxies(names: &[String]) {
    for name in names {
        eprintln!("Stopped proxy \"{}\".", name);
    }
}

pub fn no_proxies_to_restart() {
    eprintln!("No running proxies found to restart.");
}

// ── Relay lifecycle ──────────────────────────────────────────────────

pub fn relay_stopping(pid: u32) {
    eprintln!("Stopping relay (pid {pid})...");
}

pub fn relay_stopped_done() {
    eprintln!("Stopped.");
}

pub fn relay_stale_cleaned() {
    eprintln!("Cleaned up stale lock for relay.");
}

pub fn relay_restarted() {
    eprintln!("Restarted relay.");
}

pub fn relay_not_running() {
    eprintln!("Relay is not running.");
}

pub fn relay_status(info: &crate::logic::relay::RelayStatusInfo) {
    let uptime = chrono::Utc::now().timestamp() - info.started_at;
    eprintln!("Relay is running.");
    eprintln!("  PID:     {}", info.pid);
    eprintln!("  Port:    {}", info.port);
    eprintln!("  Uptime:  {}s", uptime);
}

// ── Proxy list ────────────────────────────────────────────────────────

pub fn proxy_list(proxies: &[(String, LockStatus)], mode: OutputMode) {
    if proxies.is_empty() {
        if mode == OutputMode::Json {
            println!("[]");
        } else {
            eprintln!("No proxies found.");
        }
        return;
    }

    if mode == OutputMode::Json {
        let items: Vec<serde_json::Value> = proxies
            .iter()
            .map(|(name, status)| match status {
                LockStatus::Held(info) => serde_json::json!({
                    "name": name,
                    "status": "running",
                    "pid": info.pid,
                    "port": info.port,
                    "started_at": info.started_at,
                    "config_path": info.config_path,
                }),
                LockStatus::Stale(info) => serde_json::json!({
                    "name": name,
                    "status": "stale",
                    "pid": info.pid,
                    "port": info.port,
                    "started_at": info.started_at,
                    "config_path": info.config_path,
                }),
                LockStatus::Free => serde_json::json!({
                    "name": name,
                    "status": "stopped",
                }),
            })
            .collect();
        println!("{}", serde_json::to_string(&items).unwrap_or_default());
        return;
    }

    println!(
        "{:<24} {:<10} {:>7}  {:>6}  {:<20}  CONFIG",
        "NAME", "STATUS", "PID", "PORT", "STARTED"
    );
    for (name, status) in proxies {
        match status {
            LockStatus::Held(info) => {
                println!(
                    "{:<24} {:<10} {:>7}  {:>6}  {:<20}  {}",
                    name,
                    "running",
                    info.pid,
                    info.port,
                    format_ts(info.started_at * 1000),
                    info.config_path,
                );
            }
            LockStatus::Stale(info) => {
                println!(
                    "{:<24} {:<10} {:>7}  {:>6}  {:<20}  {}",
                    name,
                    "stale",
                    info.pid,
                    info.port,
                    format_ts(info.started_at * 1000),
                    info.config_path,
                );
            }
            LockStatus::Free => {
                println!(
                    "{:<24} {:<10} {:>7}  {:>6}  {:<20}  —",
                    name, "stopped", "—", "—", "—"
                );
            }
        }
    }

    let running = proxies
        .iter()
        .filter(|(_, s)| matches!(s, LockStatus::Held(_)))
        .count();
    let total = proxies.len();
    println!();
    println!("{running} running, {total} total");
}

// ── Logs ──────────────────────────────────────────────────────────────

/// Print the log table header.
pub fn logs_header() {
    println!(
        "{:<21} {:<28} {:<32} {:>8}  {:>7}  {:>7}  {:>8}  STATUS",
        "TIME", "METHOD", "TOOL", "LATENCY", "IN", "OUT", "ERR"
    );
}

/// Render log rows (used for both initial display and follow-mode batches).
pub fn log_rows(rows: &[LogRow], mode: OutputMode) {
    if mode == OutputMode::Json {
        for row in rows {
            print_json(row);
        }
        return;
    }
    for row in rows {
        log_row(row);
    }
}

/// Render a single log row in pretty mode.
fn log_row(row: &LogRow) {
    let tool = row.tool.as_deref().unwrap_or("—");
    let status_str = match row.status.as_str() {
        "error" => format!("error  {:?}", row.error_msg.as_deref().unwrap_or("")),
        s => s.to_string(),
    };
    let in_str = format_bytes_col(row.bytes_in);
    let out_str = format_bytes_col(row.bytes_out);
    let err_str = row.error_code.as_deref().unwrap_or("");
    let line = format!(
        "{:<21} {:<28} {:<32} {:>8}  {:>7}  {:>7}  {:>8}  {}",
        format_ts(row.ts),
        row.method,
        tool,
        format_latency(row.latency_us),
        in_str,
        out_str,
        err_str,
        status_str,
    );
    if row.error_code.is_some() {
        println!("{}", colored::Colorize::red(line.as_str()));
    } else {
        println!("{line}");
    }
}

pub fn logs_empty() {
    println!("  (no records found)");
}

// ── Slow calls ────────────────────────────────────────────────────────

pub fn slow_header(proxy: &Option<String>, since: &str, threshold: &str) {
    println!(
        "TOP SLOW CALLS — {} — last {} (threshold: {})\n",
        proxy_display(proxy),
        since,
        threshold
    );
    println!(
        "  {:<32} {:>10}  {:>9}   {:<21}  STATUS",
        "TOOL", "LATENCY", "SIZE", "TIME"
    );
}

pub fn slow_rows(rows: &[LogRow], mode: OutputMode) {
    if mode == OutputMode::Json {
        for row in rows {
            print_json(row);
        }
        return;
    }
    for row in rows {
        slow_row(row);
    }
}

fn slow_row(row: &LogRow) {
    let tool = row.tool.as_deref().unwrap_or(&row.method);
    let bytes_total = row.bytes_in.unwrap_or(0).max(0) + row.bytes_out.unwrap_or(0).max(0);
    let size_str = if bytes_total > 0 {
        format_bytes(bytes_total as u64)
    } else {
        "—".to_string()
    };
    println!(
        "  {:<32} {:>10}  {:>9}   {:<21}  {}",
        tool,
        format_latency(row.latency_us),
        size_str,
        format_ts(row.ts),
        row.status,
    );
}

pub fn slow_empty() {
    println!("  (no slow calls found)");
}

pub fn slow_summary(rows: &[LogRow], since: &str) {
    if !rows.is_empty() {
        let avg: i64 = rows.iter().map(|r| r.latency_us).sum::<i64>() / rows.len() as i64;
        println!(
            "\n  {} calls above threshold in last {} (avg: {})",
            rows.len(),
            since,
            format_latency(avg),
        );
    }
}

// ── Stats ─────────────────────────────────────────────────────────────

pub fn stats(result: &StatsResult, proxy: &Option<String>, since: &str, mode: OutputMode) {
    if mode == OutputMode::Json {
        print_json(result);
        return;
    }

    println!(
        "STATS — {} — last {}   Total: {} calls   Errors: {:.1}%\n",
        proxy_display(proxy),
        since,
        result.total_calls,
        result.error_pct
    );
    println!(
        "  {:<22} {:>6}  {:>7}  {:>7}  {:>7}  {:>8}  {:>9}  {:>9}  {:>9}",
        "TOOL", "CALLS", "AVG", "P95", "MAX", "ERRORS", "BYTES IN", "BYTES OUT", "AVG SIZE"
    );
    for t in &result.tools {
        let error_str = if t.error_pct > 0.0 {
            format!("{:.1}%", t.error_pct)
        } else {
            "0%".to_string()
        };
        let bytes_in = t.total_bytes_in.max(0) as u64;
        let bytes_out = t.total_bytes_out.max(0) as u64;
        let in_str = if bytes_in > 0 {
            format_bytes(bytes_in)
        } else {
            "—".to_string()
        };
        let out_str = if bytes_out > 0 {
            format_bytes(bytes_out)
        } else {
            "—".to_string()
        };
        let avg_size = if t.calls > 0 {
            format_bytes((bytes_in + bytes_out) / t.calls as u64)
        } else {
            "—".to_string()
        };
        println!(
            "  {:<22} {:>6}  {:>7}  {:>7}  {:>7}  {:>8}  {:>9}  {:>9}  {:>9}",
            t.label,
            t.calls,
            format_latency(t.avg_us as i64),
            format_latency(t.p95_us),
            format_latency(t.max_us),
            error_str,
            in_str,
            out_str,
            avg_size,
        );
    }
    if result.tools.is_empty() {
        println!("  (no data)");
    }
}

// ── Sessions ──────────────────────────────────────────────────────────

pub fn sessions(rows: &[SessionRow], proxy: &Option<String>, since: &str, mode: OutputMode) {
    if mode == OutputMode::Json {
        for row in rows {
            print_json(row);
        }
        return;
    }

    println!("SESSIONS — {} — last {}\n", proxy_display(proxy), since);
    println!(
        "  {:<10} {:<24} {:<17} {:>12} {:>6} {:>6}",
        "SESSION", "CLIENT", "STARTED", "LAST SEEN", "CALLS", "ERRS"
    );
    for row in rows {
        let client = match (&row.client_name, &row.client_version) {
            (Some(n), Some(v)) => format!("{n} {v}"),
            (Some(n), None) => n.clone(),
            _ => "unknown".to_string(),
        };
        let status_icon = if row.is_active { "●" } else { "○" };
        let short_id = &row.session_id[..row.session_id.len().min(8)];
        println!(
            "  {:<10} {:<24} {:<17} {:>12} {:>6} {:>6}",
            short_id,
            format!("{client} {status_icon}"),
            format_ts(row.started_at),
            if row.is_active {
                "just now".to_string()
            } else {
                format_ts(row.last_seen_at)
            },
            row.total_calls,
            row.total_errors,
        );
    }
    let active_count = rows.iter().filter(|r| r.is_active).count();
    println!(
        "\n  {} sessions total   {} active",
        rows.len(),
        active_count
    );
}

// ── Clients ───────────────────────────────────────────────────────────

pub fn clients(rows: &[ClientRow], proxy: &Option<String>, since: &str, mode: OutputMode) {
    if mode == OutputMode::Json {
        for row in rows {
            print_json(row);
        }
        return;
    }

    println!("CLIENTS — {} — last {}\n", proxy_display(proxy), since);
    println!(
        "  {:<20} {:<10} {:<10} {:>8} {:>8} {:>8}",
        "CLIENT", "VERSION", "PLATFORM", "SESSIONS", "CALLS", "ERRORS"
    );
    for row in rows {
        println!(
            "  {:<20} {:<10} {:<10} {:>8} {:>8} {:>8}",
            row.client_name.as_deref().unwrap_or("unknown"),
            row.client_version.as_deref().unwrap_or("—"),
            row.client_platform.as_deref().unwrap_or("unknown"),
            row.sessions,
            row.total_calls,
            row.total_errors,
        );
    }
    if rows.is_empty() {
        println!("  (no clients found)");
    } else {
        println!(
            "\n  {} unique clients   {} sessions total",
            rows.len(),
            rows.iter().map(|r| r.sessions).sum::<i64>()
        );
    }
}

// ── Proxy status (overview) ──────────────────────────────────────────

/// Render the running-proxies header for `mcpr proxy status`.
pub fn status_running_proxies(running: &[&(String, LockStatus)]) {
    if running.is_empty() {
        return;
    }
    println!(
        "  {:<16} {:>8} {:>8} {:>10}",
        "PROXY", "PORT", "PID", "UPTIME"
    );
    for (name, status) in running {
        if let LockStatus::Held(info) = status {
            let uptime_secs = chrono::Utc::now().timestamp() - info.started_at;
            let uptime = if uptime_secs >= 3600 {
                format!("{}h {}m", uptime_secs / 3600, (uptime_secs % 3600) / 60)
            } else if uptime_secs >= 60 {
                format!("{}m {}s", uptime_secs / 60, uptime_secs % 60)
            } else {
                format!("{}s", uptime_secs)
            };
            println!(
                "  {:<16} {:>8} {:>8} {:>10}",
                name,
                format!(":{}", info.port),
                info.pid,
                uptime,
            );
        }
    }
    println!();
}

/// Render the full status overview (stats + sessions + active sessions).
pub fn status_overview(
    stats_result: &StatsResult,
    session_rows: &[SessionRow],
    proxy: &Option<String>,
    since: &str,
    mode: OutputMode,
) {
    let active_sessions = session_rows.iter().filter(|s| s.is_active).count();

    if mode == OutputMode::Json {
        let snapshot = serde_json::json!({
            "proxy": proxy,
            "since": since,
            "total_requests": stats_result.total_calls,
            "error_pct": stats_result.error_pct,
            "active_sessions": active_sessions,
            "total_sessions": session_rows.len(),
            "tools": stats_result.tools,
        });
        println!("{}", serde_json::to_string(&snapshot).unwrap_or_default());
        return;
    }

    println!("STATUS — {} — last {}\n", proxy_display(proxy), since);
    println!("  Total requests:    {}", stats_result.total_calls);
    println!("  Error rate:        {:.1}%", stats_result.error_pct);
    println!(
        "  Sessions:          {} total   {} active",
        session_rows.len(),
        active_sessions
    );

    if !stats_result.tools.is_empty() {
        println!(
            "\n  {:<24} {:>8} {:>10} {:>10} {:>10} {:>8} {:>9} {:>9} {:>9}",
            "TOOL", "CALLS", "AVG", "P95", "MAX", "ERR%", "BYTES IN", "BYTES OUT", "AVG SIZE"
        );
        for t in &stats_result.tools {
            let bytes_in = t.total_bytes_in.max(0) as u64;
            let bytes_out = t.total_bytes_out.max(0) as u64;
            let in_str = if bytes_in > 0 {
                format_bytes(bytes_in)
            } else {
                "—".to_string()
            };
            let out_str = if bytes_out > 0 {
                format_bytes(bytes_out)
            } else {
                "—".to_string()
            };
            let avg_size = if t.calls > 0 {
                format_bytes((bytes_in + bytes_out) / t.calls as u64)
            } else {
                "—".to_string()
            };
            println!(
                "  {:<24} {:>8} {:>10} {:>10} {:>10} {:>7.1}% {:>9} {:>9} {:>9}",
                t.label,
                t.calls,
                format_latency(t.avg_us as i64),
                format_latency(t.p95_us),
                format_latency(t.max_us),
                t.error_pct,
                in_str,
                out_str,
                avg_size,
            );
        }
    }

    if active_sessions > 0 {
        println!("\n  ACTIVE SESSIONS:");
        for s in session_rows.iter().filter(|s| s.is_active) {
            let client = match (&s.client_name, &s.client_version) {
                (Some(n), Some(v)) => format!("{n} {v}"),
                (Some(n), None) => n.clone(),
                _ => "unknown".to_string(),
            };
            println!(
                "    {} — {} — {} calls",
                s.session_id, client, s.total_calls
            );
        }
    }
}

// ── Session detail ────────────────────────────────────────────────────

pub fn session_detail(detail: &SessionDetail, mode: OutputMode) {
    if mode == OutputMode::Json {
        print_json(detail);
        return;
    }

    let client = match (&detail.client_name, &detail.client_version) {
        (Some(n), Some(v)) => format!("{n} {v}"),
        (Some(n), None) => n.clone(),
        _ => "unknown".to_string(),
    };
    let platform = detail.client_platform.as_deref().unwrap_or("unknown");
    let status = if detail.ended_at.is_some() {
        "closed"
    } else {
        "active"
    };

    println!("SESSION — {}\n", detail.session_id);
    println!("  Client:      {} ({})", client, platform);
    println!("  Status:      {}", status);
    println!("  Started:     {}", format_ts(detail.started_at));
    if let Some(ended) = detail.ended_at {
        println!("  Ended:       {}", format_ts(ended));
    } else {
        println!("  Last seen:   {}", format_ts(detail.last_seen_at));
    }
    println!(
        "  Calls: {}   Errors: {}",
        detail.total_calls, detail.total_errors
    );

    if !detail.requests.is_empty() {
        println!(
            "\n  {:<20} {:<28} {:<32} {:>10} {:>8}",
            "TIME", "METHOD", "TOOL", "LATENCY", "STATUS"
        );
        for r in &detail.requests {
            println!(
                "  {:<20} {:<28} {:<32} {:>10} {:>8}",
                format_ts(r.ts),
                r.method,
                r.tool.as_deref().unwrap_or("—"),
                format_latency(r.latency_us),
                r.status,
            );
        }
    } else {
        println!("\n  (no requests recorded)");
    }
}

// ── Schema ────────────────────────────────────────────────────────────

pub fn schema(rows: &[SchemaRow], status: Option<&SchemaStatusRow>, mode: OutputMode) {
    if mode == OutputMode::Json {
        for row in rows {
            print_json(row);
        }
        return;
    }

    if rows.is_empty() {
        println!(
            "  (no schema captured yet — schema is populated as responses flow through the proxy)"
        );
        return;
    }

    // Show status summary if we have an initialize payload.
    if let Some(status) = status {
        if let Some(name) = &status.server_name {
            let ver = status.server_version.as_deref().unwrap_or("?");
            let proto = status.protocol_version.as_deref().unwrap_or("?");
            println!("Server: {} v{} (MCP {})", name, ver, proto);
        }
        if !status.capabilities.is_empty() {
            println!("Capabilities: {}", status.capabilities.join(", "));
        }
        println!("Schema: {}", status.status);
        if let Some(ts) = status.last_captured_at {
            println!("Last captured: {}", format_ts(ts));
        }
        println!();
    }

    for row in rows {
        if row.method == "initialize" {
            continue; // Already shown in summary.
        }
        println!(
            "── {} ──  (captured {})",
            row.method,
            format_ts(row.captured_at)
        );
        print_schema_items(&row.payload, &row.method);
        println!();
    }
}

pub fn schema_changes(rows: &[SchemaChangeRow], mode: OutputMode) {
    if mode == OutputMode::Json {
        for row in rows {
            print_json(row);
        }
        return;
    }

    println!("  {:<21} {:<28} {:<22} ITEM", "TIME", "METHOD", "CHANGE");
    for row in rows {
        println!(
            "  {:<21} {:<28} {:<22} {}",
            format_ts(row.detected_at),
            row.method,
            row.change_type,
            row.item_name.as_deref().unwrap_or("—"),
        );
    }
    if rows.is_empty() {
        println!("  (no schema changes recorded)");
    }
}

pub fn schema_unused(
    rows: &[SchemaToolUsageRow],
    proxy: &Option<String>,
    since: &str,
    mode: OutputMode,
) {
    if rows.is_empty() {
        println!("  (no tools/list schema captured yet)");
        return;
    }

    if mode == OutputMode::Json {
        for row in rows {
            print_json(row);
        }
        return;
    }

    let unused_count = rows.iter().filter(|r| r.calls == 0).count();
    let total = rows.len();
    println!(
        "TOOL USAGE — {} — last {}   {}/{} unused\n",
        proxy_display(proxy),
        since,
        unused_count,
        total
    );
    println!(
        "  {:<28} {:>8} {:>8} {:>21}  STATUS",
        "TOOL", "CALLS", "ERRORS", "LAST CALLED"
    );
    for row in rows {
        let last_called = row
            .last_called_at
            .map(format_ts)
            .unwrap_or_else(|| "never".to_string());
        let status = if row.calls == 0 {
            "unused"
        } else if row.errors > 0 {
            "errors"
        } else {
            "ok"
        };
        let line = format!(
            "  {:<28} {:>8} {:>8} {:>21}  {}",
            row.tool_name, row.calls, row.errors, last_called, status,
        );
        if row.calls == 0 {
            println!("{}", colored::Colorize::yellow(line.as_str()));
        } else if row.errors > 0 {
            println!("{}", colored::Colorize::red(line.as_str()));
        } else {
            println!("{line}");
        }
    }
    if unused_count > 0 {
        println!(
            "\n  {} tool{} listed but never called in the last {}.",
            unused_count,
            if unused_count == 1 { "" } else { "s" },
            since,
        );
    }
}

/// Print named items from a schema payload in a human-readable format.
fn print_schema_items(payload: &str, method: &str) {
    let val: serde_json::Value = match serde_json::from_str(payload) {
        Ok(v) => v,
        Err(_) => {
            println!("  {payload}");
            return;
        }
    };

    let (array_key, label) = match method {
        "tools/list" => ("tools", "Tools"),
        "resources/list" => ("resources", "Resources"),
        "resources/templates/list" => ("resourceTemplates", "Resource Templates"),
        "prompts/list" => ("prompts", "Prompts"),
        _ => {
            println!("  {payload}");
            return;
        }
    };

    if let Some(items) = val.get(array_key).and_then(|a| a.as_array()) {
        println!("  {} ({}):", label, items.len());
        for item in items {
            let name = item.get("name").and_then(|n| n.as_str()).unwrap_or("?");
            let desc = item
                .get("description")
                .and_then(|d| d.as_str())
                .unwrap_or("");
            let desc_short: String = desc.chars().take(60).collect();
            if desc_short.is_empty() {
                println!("    {name}");
            } else {
                println!("    {name}  —  {desc_short}");
            }
        }
    }
}

// ── Store ─────────────────────────────────────────────────────────────

pub fn store_stats(stats_result: &StoreStats, db_path: &Path) {
    println!("STORAGE — {}\n", db_path.display());
    println!("  Total requests:    {}", stats_result.total_requests);
    println!("  Total sessions:    {}", stats_result.total_sessions);
    println!("  Proxies tracked:   {}", stats_result.proxy_count);
    if let Some(oldest) = stats_result.oldest_ts {
        println!("  Oldest record:     {}", format_ts(oldest));
    }
    if let Some(newest) = stats_result.newest_ts {
        println!("  Newest record:     {}", format_ts(newest));
    }
    println!();
    println!(
        "  Database file:     {}",
        format_bytes(stats_result.db_file_size)
    );
    println!(
        "  WAL file:          {}",
        format_bytes(stats_result.wal_file_size)
    );

    if stats_result.db_file_size > 500 * 1024 * 1024 {
        println!("\n  Run `mcpr store vacuum --before 7d` to remove records older than 7 days.");
    }
}

pub fn store_vacuum(result: &VacuumResult, dry_run: bool) {
    if dry_run {
        println!("DRY RUN — no changes made\n");
        println!("  Would delete: {} requests", result.deleted_requests);
        println!(
            "  Would delete: {} orphaned sessions",
            result.deleted_sessions
        );
        println!("\n  Run without --dry-run to apply.");
    } else {
        println!("  Deleted {} requests.", result.deleted_requests);
        println!("  Deleted {} orphaned sessions.", result.deleted_sessions);
        println!("  Disk space reclaimed via VACUUM.");
    }
}

// ── Tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;

    #[test]
    fn format_latency__sub_ms() {
        assert_eq!(format_latency(142), "142μs");
        assert_eq!(format_latency(0), "0μs");
        assert_eq!(format_latency(999), "999μs");
    }

    #[test]
    fn format_latency__ms_range() {
        assert_eq!(format_latency(1_000), "1.00ms");
        assert_eq!(format_latency(4_201), "4.20ms");
        assert_eq!(format_latency(142_000), "142.00ms");
    }

    #[test]
    fn format_latency__over_1s() {
        assert_eq!(format_latency(1_000_000), "1,000ms");
        assert_eq!(format_latency(4_201_000), "4,201ms");
        assert_eq!(format_latency(12_345_000), "12,345ms");
    }

    #[test]
    fn format_latency__boundary_us_to_ms() {
        assert_eq!(format_latency(999), "999μs");
        assert_eq!(format_latency(1_000), "1.00ms");
    }

    #[test]
    fn format_latency__boundary_ms_to_s() {
        assert_eq!(format_latency(999_999), "1000.00ms");
        assert_eq!(format_latency(1_000_000), "1,000ms");
    }

    #[test]
    fn format_latency__fractional_ms() {
        assert_eq!(format_latency(1_500), "1.50ms");
        assert_eq!(format_latency(10_250), "10.25ms");
        assert_eq!(format_latency(500_000), "500.00ms");
    }

    #[test]
    fn format_bytes__units() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1024), "1.0 KB");
        assert_eq!(format_bytes(1024 * 1024), "1.0 MB");
        assert_eq!(format_bytes(1024 * 1024 * 1024), "1.0 GB");
    }

    #[test]
    fn format_bytes_col__none() {
        assert_eq!(format_bytes_col(None), "—");
    }

    #[test]
    fn format_bytes_col__zero() {
        assert_eq!(format_bytes_col(Some(0)), "—");
    }

    #[test]
    fn format_bytes_col__negative() {
        assert_eq!(format_bytes_col(Some(-1)), "—");
    }

    #[test]
    fn format_bytes_col__positive() {
        assert_eq!(format_bytes_col(Some(512)), "512 B");
        assert_eq!(format_bytes_col(Some(2048)), "2.0 KB");
        assert_eq!(format_bytes_col(Some(1_500_000)), "1.4 MB");
    }

    #[test]
    fn format_ts__valid() {
        let ts = 1712345678000_i64; // 2024-04-05T18:34:38Z
        let result = format_ts(ts);
        assert_ne!(result, "?");
        assert!(result.contains("2024"));
    }

    #[test]
    fn format_ts__zero() {
        let result = format_ts(0);
        assert_ne!(result, "?"); // epoch is valid
    }

    // ── Relay render functions ────────────────────────────────────────

    #[test]
    fn relay_stopping__does_not_panic() {
        relay_stopping(12345);
    }

    #[test]
    fn relay_stopped_done__does_not_panic() {
        relay_stopped_done();
    }

    #[test]
    fn relay_stale_cleaned__does_not_panic() {
        relay_stale_cleaned();
    }

    #[test]
    fn relay_restarted__does_not_panic() {
        relay_restarted();
    }

    #[test]
    fn relay_not_running__does_not_panic() {
        relay_not_running();
    }

    #[test]
    fn relay_status__formats_output() {
        use crate::logic::relay::RelayStatusInfo;
        let info = RelayStatusInfo {
            pid: 12345,
            port: 8080,
            started_at: chrono::Utc::now().timestamp() - 3600,
        };
        // Should not panic and should produce output.
        relay_status(&info);
    }
}
