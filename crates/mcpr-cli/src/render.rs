//! Terminal rendering layer.
//!
//! Every function in this module receives **data** and writes it to
//! stdout/stderr.  No database access, no process management — just
//! formatting and printing.  Commands call into this module after
//! obtaining data from `logic::*`.

use std::path::Path;

use mcpr_integrations::store::query::{
    clients::ClientRow,
    logs::LogRow,
    schema::{SchemaChangeRow, SchemaRow, SchemaStatusRow, SchemaToolUsageRow},
    session_detail::SessionDetail,
    sessions::SessionRow,
    stats::StatsResult,
    store_ops::{StoreStats, VacuumResult},
};
use tabled::{
    Table, Tabled,
    settings::{Alignment, Style, object::Columns},
};

use crate::proxy_lock::{LockInfo, LockStatus};

/// Render a `Tabled`-derived row set as a borderless table with numeric columns
/// (everything past the first) right-aligned.
fn render_table<T: Tabled>(rows: Vec<T>) -> String {
    let mut t = Table::new(rows);
    t.with(Style::blank())
        .modify(Columns::new(1..), Alignment::right());
    t.to_string()
}

/// Percent-encode a URL for use as a query parameter value.
fn encode_uri_component(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

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
    mcpr_core::utils::time::format_latency_us(us)
}

/// Print a serializable struct as a single JSON line.
pub fn print_json(value: &impl serde::Serialize) {
    if let Ok(json) = serde_json::to_string(value) {
        println!("{json}");
    }
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

// ── Proxy lifecycle ───────────────────────────────────────────────────

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

pub fn proxy_deleted(name: &str) {
    eprintln!("Deleted proxy \"{}\".", name);
}

pub fn proxy_delete_cancelled(name: &str) {
    eprintln!("Cancelled — proxy \"{}\" was not deleted.", name);
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

#[derive(Tabled)]
struct ProxyListRow {
    #[tabled(rename = "NAME")]
    name: String,
    #[tabled(rename = "STATUS")]
    status: String,
    #[tabled(rename = "PID")]
    pid: String,
    #[tabled(rename = "PORT")]
    port: String,
    #[tabled(rename = "STARTED")]
    started: String,
}

impl ProxyListRow {
    fn from_status(name: &str, status: &LockStatus) -> Self {
        match status {
            LockStatus::Held(info) => Self {
                name: name.to_string(),
                status: "running".to_string(),
                pid: info.pid.to_string(),
                port: info.port.to_string(),
                started: format_ts(info.started_at * 1000),
            },
            LockStatus::Stale(info) => Self {
                name: name.to_string(),
                status: "stale".to_string(),
                pid: info.pid.to_string(),
                port: info.port.to_string(),
                started: format_ts(info.started_at * 1000),
            },
            LockStatus::Free => Self {
                name: name.to_string(),
                status: "stopped".to_string(),
                pid: "—".to_string(),
                port: "—".to_string(),
                started: "—".to_string(),
            },
        }
    }
}

/// URLs + config path for a single running proxy, shown below the list table.
fn print_proxy_detail(name: &str, info: &LockInfo) {
    let localhost = format!("http://localhost:{}", info.port);
    let tunnel = crate::proxy_lock::read_tunnel_url(name).filter(|t| *t != localhost);
    let upstream = crate::proxy_lock::read_upstream_url(name);

    println!();
    println!("  {name}");
    println!("    local:    {localhost}");
    if let Some(t) = &tunnel {
        println!("    tunnel:   {t}");
    }
    if let Some(u) = &upstream {
        println!("    upstream: {u}");
    }
    println!("    config:   {}", info.config_path);
}

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
                LockStatus::Held(info) => {
                    let localhost = format!("http://localhost:{}", info.port);
                    let tunnel =
                        crate::proxy_lock::read_tunnel_url(name).filter(|t| *t != localhost);
                    let upstream = crate::proxy_lock::read_upstream_url(name);
                    serde_json::json!({
                        "name": name,
                        "status": "running",
                        "pid": info.pid,
                        "port": info.port,
                        "started_at": info.started_at,
                        "config_path": info.config_path,
                        "localhost_url": localhost,
                        "tunnel_url": tunnel,
                        "upstream_url": upstream,
                    })
                }
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

    let rows: Vec<_> = proxies
        .iter()
        .map(|(name, status)| ProxyListRow::from_status(name, status))
        .collect();
    println!("{}", render_table(rows));

    for (name, status) in proxies {
        if let LockStatus::Held(info) = status {
            print_proxy_detail(name, info);
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

#[derive(Tabled)]
struct LogDisplayRow {
    #[tabled(rename = "TIME")]
    time: String,
    #[tabled(rename = "METHOD")]
    method: String,
    #[tabled(rename = "TOOL")]
    tool: String,
    #[tabled(rename = "URI")]
    resource_uri: String,
    #[tabled(rename = "PROMPT")]
    prompt: String,
    #[tabled(rename = "LATENCY")]
    latency: String,
    #[tabled(rename = "IN")]
    bytes_in: String,
    #[tabled(rename = "OUT")]
    bytes_out: String,
    #[tabled(rename = "ERR")]
    err: String,
    #[tabled(rename = "STATUS")]
    status: String,
}

impl From<&LogRow> for LogDisplayRow {
    fn from(row: &LogRow) -> Self {
        let status = match row.status.as_str() {
            "error" => format!("error  {:?}", row.error_msg.as_deref().unwrap_or("")),
            s => s.to_string(),
        };
        Self {
            time: format_ts(row.ts),
            method: row.method.clone(),
            tool: row.tool.clone().unwrap_or_else(|| "—".to_string()),
            resource_uri: row
                .resource_uri
                .as_deref()
                .map(truncate_uri)
                .unwrap_or_else(|| "—".to_string()),
            prompt: row.prompt_name.clone().unwrap_or_else(|| "—".to_string()),
            latency: format_latency(row.latency_us),
            bytes_in: format_bytes_col(row.bytes_in),
            bytes_out: format_bytes_col(row.bytes_out),
            err: row.error_code.clone().unwrap_or_default(),
            status,
        }
    }
}

/// URIs can be 100+ chars and blow up table width. Trim the middle so the
/// scheme prefix and the trailing path segment stay visible.
fn truncate_uri(uri: &str) -> String {
    const MAX: usize = 50;
    if uri.chars().count() <= MAX {
        return uri.to_string();
    }
    let head: String = uri.chars().take(24).collect();
    let tail: String = uri
        .chars()
        .rev()
        .take(23)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    format!("{head}…{tail}")
}

/// Render log rows. In pretty mode, error rows are colored red.
/// Set `with_header = false` for follow-mode batches.
pub fn log_rows(rows: &[LogRow], mode: OutputMode, with_header: bool) {
    if mode == OutputMode::Json {
        for row in rows {
            print_json(row);
        }
        return;
    }
    if rows.is_empty() {
        return;
    }
    let display: Vec<LogDisplayRow> = rows.iter().map(LogDisplayRow::from).collect();
    let table = render_table(display);
    let mut lines = table.lines();
    if let Some(header) = lines.next()
        && with_header
    {
        println!("{header}");
    }
    for (i, line) in lines.enumerate() {
        if rows.get(i).and_then(|r| r.error_code.as_ref()).is_some() {
            println!("{}", colored::Colorize::red(line));
        } else {
            println!("{line}");
        }
    }
}

pub fn logs_empty() {
    println!("  (no records found)");
}

// ── Slow calls ────────────────────────────────────────────────────────

#[derive(Tabled)]
struct SlowDisplayRow {
    #[tabled(rename = "TOOL")]
    tool: String,
    #[tabled(rename = "LATENCY")]
    latency: String,
    #[tabled(rename = "SIZE")]
    size: String,
    #[tabled(rename = "TIME")]
    time: String,
    #[tabled(rename = "STATUS")]
    status: String,
}

impl From<&LogRow> for SlowDisplayRow {
    fn from(row: &LogRow) -> Self {
        let bytes_total = row.bytes_in.unwrap_or(0).max(0) + row.bytes_out.unwrap_or(0).max(0);
        let size = if bytes_total > 0 {
            format_bytes(bytes_total as u64)
        } else {
            "—".to_string()
        };
        Self {
            tool: row.tool.clone().unwrap_or_else(|| row.method.clone()),
            latency: format_latency(row.latency_us),
            size,
            time: format_ts(row.ts),
            status: row.status.clone(),
        }
    }
}

pub fn slow_banner(proxy: &Option<String>, since: &str, threshold: &str) {
    println!(
        "TOP SLOW CALLS — {} — last {} (threshold: {})\n",
        proxy_display(proxy),
        since,
        threshold
    );
}

pub fn slow_rows(rows: &[LogRow], mode: OutputMode, with_header: bool) {
    if mode == OutputMode::Json {
        for row in rows {
            print_json(row);
        }
        return;
    }
    if rows.is_empty() {
        return;
    }
    let display: Vec<SlowDisplayRow> = rows.iter().map(SlowDisplayRow::from).collect();
    let table = render_table(display);
    let mut lines = table.lines();
    if let Some(header) = lines.next()
        && with_header
    {
        println!("{header}");
    }
    for line in lines {
        println!("{line}");
    }
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

#[derive(Tabled)]
struct ToolStatsRow {
    #[tabled(rename = "TOOL")]
    tool: String,
    #[tabled(rename = "CALLS")]
    calls: i64,
    #[tabled(rename = "AVG")]
    avg: String,
    #[tabled(rename = "P95")]
    p95: String,
    #[tabled(rename = "MAX")]
    max: String,
    #[tabled(rename = "ERRORS")]
    errors: String,
    #[tabled(rename = "BYTES IN")]
    bytes_in: String,
    #[tabled(rename = "BYTES OUT")]
    bytes_out: String,
    #[tabled(rename = "AVG SIZE")]
    avg_size: String,
}

impl From<&mcpr_integrations::store::query::stats::ToolStats> for ToolStatsRow {
    fn from(t: &mcpr_integrations::store::query::stats::ToolStats) -> Self {
        let bytes_in = t.total_bytes_in.max(0) as u64;
        let bytes_out = t.total_bytes_out.max(0) as u64;
        let fmt_bytes = |b: u64| {
            if b > 0 {
                format_bytes(b)
            } else {
                "—".to_string()
            }
        };
        let avg_size = if t.calls > 0 {
            format_bytes((bytes_in + bytes_out) / t.calls as u64)
        } else {
            "—".to_string()
        };
        let errors = if t.error_pct > 0.0 {
            format!("{:.1}%", t.error_pct)
        } else {
            "0%".to_string()
        };
        Self {
            tool: t.label.clone(),
            calls: t.calls,
            avg: format_latency(t.avg_us as i64),
            p95: format_latency(t.p95_us),
            max: format_latency(t.max_us),
            errors,
            bytes_in: fmt_bytes(bytes_in),
            bytes_out: fmt_bytes(bytes_out),
            avg_size,
        }
    }
}

// ── Sessions ──────────────────────────────────────────────────────────

#[derive(Tabled)]
struct SessionDisplayRow {
    #[tabled(rename = "SESSION")]
    session: String,
    #[tabled(rename = "CLIENT")]
    client: String,
    #[tabled(rename = "STARTED")]
    started: String,
    #[tabled(rename = "LAST SEEN")]
    last_seen: String,
    #[tabled(rename = "CALLS")]
    calls: i64,
    #[tabled(rename = "ERRS")]
    errors: i64,
}

impl From<&SessionRow> for SessionDisplayRow {
    fn from(row: &SessionRow) -> Self {
        let client_name = match (&row.client_name, &row.client_version) {
            (Some(n), Some(v)) => format!("{n} {v}"),
            (Some(n), None) => n.clone(),
            _ => "unknown".to_string(),
        };
        let status_icon = if row.is_active { "●" } else { "○" };
        let short_id = row.session_id[..row.session_id.len().min(8)].to_string();
        Self {
            session: short_id,
            client: format!("{client_name} {status_icon}"),
            started: format_ts(row.started_at),
            last_seen: if row.is_active {
                "just now".to_string()
            } else {
                format_ts(row.last_seen_at)
            },
            calls: row.total_calls,
            errors: row.total_errors,
        }
    }
}

pub fn sessions(rows: &[SessionRow], proxy: &Option<String>, since: &str, mode: OutputMode) {
    if mode == OutputMode::Json {
        for row in rows {
            print_json(row);
        }
        return;
    }

    println!("SESSIONS — {} — last {}\n", proxy_display(proxy), since);
    if !rows.is_empty() {
        let display: Vec<SessionDisplayRow> = rows.iter().map(SessionDisplayRow::from).collect();
        println!("{}", render_table(display));
    }
    let active_count = rows.iter().filter(|r| r.is_active).count();
    println!(
        "\n  {} sessions total   {} active",
        rows.len(),
        active_count
    );
}

// ── Clients ───────────────────────────────────────────────────────────

#[derive(Tabled)]
struct ClientDisplayRow {
    #[tabled(rename = "CLIENT")]
    client: String,
    #[tabled(rename = "VERSION")]
    version: String,
    #[tabled(rename = "PLATFORM")]
    platform: String,
    #[tabled(rename = "SESSIONS")]
    sessions: i64,
    #[tabled(rename = "CALLS")]
    calls: i64,
    #[tabled(rename = "ERRORS")]
    errors: i64,
}

impl From<&ClientRow> for ClientDisplayRow {
    fn from(row: &ClientRow) -> Self {
        Self {
            client: row.client_name.clone().unwrap_or_else(|| "unknown".into()),
            version: row.client_version.clone().unwrap_or_else(|| "—".into()),
            platform: row
                .client_platform
                .clone()
                .unwrap_or_else(|| "unknown".into()),
            sessions: row.sessions,
            calls: row.total_calls,
            errors: row.total_errors,
        }
    }
}

pub fn clients(rows: &[ClientRow], proxy: &Option<String>, since: &str, mode: OutputMode) {
    if mode == OutputMode::Json {
        for row in rows {
            print_json(row);
        }
        return;
    }

    println!("CLIENTS — {} — last {}\n", proxy_display(proxy), since);
    if rows.is_empty() {
        println!("  (no clients found)");
        return;
    }
    let display: Vec<ClientDisplayRow> = rows.iter().map(ClientDisplayRow::from).collect();
    println!("{}", render_table(display));
    println!(
        "\n  {} unique clients   {} sessions total",
        rows.len(),
        rows.iter().map(|r| r.sessions).sum::<i64>()
    );
}

// ── Proxy status (overview) ──────────────────────────────────────────

#[derive(Tabled)]
struct RunningProxyRow {
    #[tabled(rename = "PROXY")]
    proxy: String,
    #[tabled(rename = "STATUS")]
    status: String,
    #[tabled(rename = "PORT")]
    port: String,
    #[tabled(rename = "PID")]
    pid: String,
    #[tabled(rename = "UPTIME")]
    uptime: String,
}

fn status_label(status: &LockStatus) -> &'static str {
    match status {
        LockStatus::Held(_) => "running",
        LockStatus::Stale(_) => "stale",
        LockStatus::Free => "stopped",
    }
}

fn format_uptime(seconds: i64) -> String {
    if seconds >= 3600 {
        format!("{}h {}m", seconds / 3600, (seconds % 3600) / 60)
    } else if seconds >= 60 {
        format!("{}m {}s", seconds / 60, seconds % 60)
    } else {
        format!("{seconds}s")
    }
}

/// Render the proxies header for `mcpr proxy status` — one row per known
/// proxy with its current state (running / stopped / stale), followed by
/// URL details for the running ones.
pub fn status_proxies(proxies: &[&(String, LockStatus)]) {
    if proxies.is_empty() {
        return;
    }

    let now = chrono::Utc::now().timestamp();
    let rows: Vec<RunningProxyRow> = proxies
        .iter()
        .map(|(name, status)| {
            let label = status_label(status).to_string();
            match status {
                LockStatus::Held(info) | LockStatus::Stale(info) => RunningProxyRow {
                    proxy: name.clone(),
                    status: label,
                    port: format!(":{}", info.port),
                    pid: info.pid.to_string(),
                    uptime: if matches!(status, LockStatus::Held(_)) {
                        format_uptime(now - info.started_at)
                    } else {
                        "—".to_string()
                    },
                },
                LockStatus::Free => RunningProxyRow {
                    proxy: name.clone(),
                    status: label,
                    port: "—".to_string(),
                    pid: "—".to_string(),
                    uptime: "—".to_string(),
                },
            }
        })
        .collect();
    println!("{}", render_table(rows));

    for (name, status) in proxies {
        if let LockStatus::Held(info) = status {
            let localhost_url = format!("http://localhost:{}", info.port);
            let encoded_localhost = encode_uri_component(&localhost_url);
            let tunnel_url = crate::proxy_lock::read_tunnel_url(name);

            println!();
            println!("  {name}");
            println!("    localhost: {localhost_url}");
            println!("    studio:    https://cloud.mcpr.app/studio?proxy={encoded_localhost}");
            if let Some(ref turl) = tunnel_url
                && *turl != localhost_url
            {
                let encoded_tunnel = encode_uri_component(turl);
                println!("    tunnel:    {turl}");
                println!("    studio:    https://cloud.mcpr.app/studio?proxy={encoded_tunnel}");
            }
        }
    }
    println!();
}

/// Render the full status overview (stats + sessions + active sessions).
pub fn status_overview(
    stats_result: &StatsResult,
    session_rows: &[SessionRow],
    proxies: &[&(String, LockStatus)],
    proxy: &Option<String>,
    since: &str,
    mode: OutputMode,
) {
    let active_sessions = session_rows.iter().filter(|s| s.is_active).count();

    if mode == OutputMode::Json {
        let proxies_json: Vec<_> = proxies
            .iter()
            .map(|(name, status)| match status {
                LockStatus::Held(info) => {
                    let localhost = format!("http://localhost:{}", info.port);
                    let tunnel =
                        crate::proxy_lock::read_tunnel_url(name).filter(|t| *t != localhost);
                    let tunnel_studio = tunnel
                        .as_ref()
                        .map(|t| format!("https://cloud.mcpr.app/studio?proxy={}", encode_uri_component(t)));
                    serde_json::json!({
                        "name": name,
                        "status": "running",
                        "port": info.port,
                        "pid": info.pid,
                        "localhost_url": localhost,
                        "studio_url": format!("https://cloud.mcpr.app/studio?proxy={}", encode_uri_component(&localhost)),
                        "tunnel_url": tunnel,
                        "tunnel_studio_url": tunnel_studio,
                    })
                }
                LockStatus::Stale(info) => serde_json::json!({
                    "name": name,
                    "status": "stale",
                    "port": info.port,
                    "pid": info.pid,
                }),
                LockStatus::Free => serde_json::json!({
                    "name": name,
                    "status": "stopped",
                }),
            })
            .collect();

        let snapshot = serde_json::json!({
            "proxy": proxy,
            "since": since,
            "total_requests": stats_result.total_calls,
            "error_pct": stats_result.error_pct,
            "active_sessions": active_sessions,
            "total_sessions": session_rows.len(),
            "proxies": proxies_json,
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
        println!();
        let rows: Vec<ToolStatsRow> = stats_result.tools.iter().map(ToolStatsRow::from).collect();
        println!("{}", render_table(rows));
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
        println!();
        let rows: Vec<SessionRequestRow> = detail
            .requests
            .iter()
            .map(SessionRequestRow::from)
            .collect();
        println!("{}", render_table(rows));
    } else {
        println!("\n  (no requests recorded)");
    }
}

#[derive(Tabled)]
struct SessionRequestRow {
    #[tabled(rename = "TIME")]
    time: String,
    #[tabled(rename = "METHOD")]
    method: String,
    #[tabled(rename = "TOOL")]
    tool: String,
    #[tabled(rename = "LATENCY")]
    latency: String,
    #[tabled(rename = "STATUS")]
    status: String,
}

impl From<&LogRow> for SessionRequestRow {
    fn from(r: &LogRow) -> Self {
        Self {
            time: format_ts(r.ts),
            method: r.method.clone(),
            tool: r.tool.clone().unwrap_or_else(|| "—".to_string()),
            latency: format_latency(r.latency_us),
            status: r.status.clone(),
        }
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

#[derive(Tabled)]
struct SchemaChangeDisplayRow {
    #[tabled(rename = "TIME")]
    time: String,
    #[tabled(rename = "METHOD")]
    method: String,
    #[tabled(rename = "CHANGE")]
    change: String,
    #[tabled(rename = "ITEM")]
    item: String,
}

impl From<&SchemaChangeRow> for SchemaChangeDisplayRow {
    fn from(row: &SchemaChangeRow) -> Self {
        Self {
            time: format_ts(row.detected_at),
            method: row.method.clone(),
            change: row.change_type.clone(),
            item: row.item_name.clone().unwrap_or_else(|| "—".into()),
        }
    }
}

pub fn schema_changes(rows: &[SchemaChangeRow], mode: OutputMode) {
    if mode == OutputMode::Json {
        for row in rows {
            print_json(row);
        }
        return;
    }

    if rows.is_empty() {
        println!("  (no schema changes recorded)");
        return;
    }
    let display: Vec<SchemaChangeDisplayRow> =
        rows.iter().map(SchemaChangeDisplayRow::from).collect();
    println!("{}", render_table(display));
}

#[derive(Tabled)]
struct SchemaUsageDisplayRow {
    #[tabled(rename = "TOOL")]
    tool: String,
    #[tabled(rename = "CALLS")]
    calls: i64,
    #[tabled(rename = "ERRORS")]
    errors: i64,
    #[tabled(rename = "LAST CALLED")]
    last_called: String,
    #[tabled(rename = "STATUS")]
    status: String,
}

impl From<&SchemaToolUsageRow> for SchemaUsageDisplayRow {
    fn from(row: &SchemaToolUsageRow) -> Self {
        let status = if row.calls == 0 {
            "unused"
        } else if row.errors > 0 {
            "errors"
        } else {
            "ok"
        };
        Self {
            tool: row.tool_name.clone(),
            calls: row.calls,
            errors: row.errors,
            last_called: row
                .last_called_at
                .map(format_ts)
                .unwrap_or_else(|| "never".to_string()),
            status: status.to_string(),
        }
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
    let display: Vec<SchemaUsageDisplayRow> =
        rows.iter().map(SchemaUsageDisplayRow::from).collect();
    let table = render_table(display);
    let mut lines = table.lines();
    if let Some(header) = lines.next() {
        println!("{header}");
    }
    for (i, line) in lines.enumerate() {
        let row = &rows[i];
        if row.calls == 0 {
            println!("{}", colored::Colorize::yellow(line));
        } else if row.errors > 0 {
            println!("{}", colored::Colorize::red(line));
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
    fn truncate_uri__short_unchanged() {
        assert_eq!(truncate_uri("file:///etc/hosts"), "file:///etc/hosts");
    }

    #[test]
    fn truncate_uri__exactly_max_unchanged() {
        let uri: String = std::iter::repeat_n('a', 50).collect();
        assert_eq!(truncate_uri(&uri), uri);
    }

    #[test]
    fn truncate_uri__long_keeps_head_and_tail() {
        let uri = "https://very.long.example.com/api/v2/resources/abc123/items/xyz789";
        let out = truncate_uri(uri);
        assert!(out.contains('…'));
        assert!(out.starts_with("https://very.long.exampl"));
        assert!(out.ends_with("/items/xyz789"));
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

    // ── status_proxies ────────────────────────────────────────────────

    fn held_proxy(name: &str, port: u16) -> (String, LockStatus) {
        (
            name.to_string(),
            LockStatus::Held(LockInfo {
                pid: std::process::id(),
                port,
                started_at: chrono::Utc::now().timestamp() - 120,
                config_path: "/tmp/test.toml".to_string(),
            }),
        )
    }

    fn stopped_proxy(name: &str) -> (String, LockStatus) {
        (name.to_string(), LockStatus::Free)
    }

    #[test]
    fn status_proxies__empty_is_noop() {
        status_proxies(&[]);
    }

    #[test]
    fn status_proxies__shows_running_proxy() {
        let proxy = held_proxy("test-proxy", 3000);
        status_proxies(&[&proxy]);
    }

    #[test]
    fn status_proxies__shows_stopped_proxy() {
        let proxy = stopped_proxy("idle-proxy");
        status_proxies(&[&proxy]);
    }

    #[test]
    fn status_proxies__mixed_running_and_stopped() {
        let a = held_proxy("alpha", 3000);
        let b = stopped_proxy("beta");
        status_proxies(&[&a, &b]);
    }

    #[test]
    fn status_label__variants() {
        let held = held_proxy("x", 1);
        assert_eq!(status_label(&held.1), "running");
        assert_eq!(status_label(&LockStatus::Free), "stopped");
        let stale = LockStatus::Stale(LockInfo {
            pid: 99999999,
            port: 1234,
            started_at: 0,
            config_path: "/x".into(),
        });
        assert_eq!(status_label(&stale), "stale");
    }

    // ── encode_uri_component ───────────────────────────────────────

    #[test]
    fn encode_uri_component__encodes_colons_and_slashes() {
        let encoded = encode_uri_component("http://localhost:3000");
        assert_eq!(encoded, "http%3A%2F%2Flocalhost%3A3000");
    }

    #[test]
    fn encode_uri_component__plain_string_unchanged() {
        assert_eq!(encode_uri_component("hello"), "hello");
    }

    // ── status_overview ──────────────────────────────────────────────

    fn empty_stats() -> StatsResult {
        StatsResult {
            total_calls: 0,
            error_pct: 0.0,
            tools: vec![],
        }
    }

    #[test]
    fn status_overview__pretty_no_sessions() {
        let proxy = held_proxy("demo", 3000);
        status_overview(
            &empty_stats(),
            &[],
            &[&proxy],
            &None,
            "1h",
            OutputMode::Pretty,
        );
    }

    #[test]
    fn status_overview__json_includes_running_proxies() {
        let proxy = held_proxy("demo", 3000);
        // JSON mode should not panic and should include running_proxies.
        status_overview(
            &empty_stats(),
            &[],
            &[&proxy],
            &None,
            "1h",
            OutputMode::Json,
        );
    }

    #[test]
    fn status_overview__json_empty_proxies() {
        status_overview(
            &empty_stats(),
            &[],
            &[],
            &Some("my-proxy".to_string()),
            "24h",
            OutputMode::Json,
        );
    }
}
