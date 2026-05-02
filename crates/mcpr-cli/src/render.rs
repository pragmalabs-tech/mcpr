//! Terminal rendering layer.
//!
//! Every function in this module receives **data** and writes it to
//! stdout/stderr.  No database access, no process management — just
//! formatting and printing.  Commands call into this module after
//! obtaining data from `logic::*`.

use std::path::Path;

use mcpr_integrations::store::query::store_ops::{StoreStats, VacuumResult};
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
    fn format_bytes__units() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1024), "1.0 KB");
        assert_eq!(format_bytes(1024 * 1024), "1.0 MB");
        assert_eq!(format_bytes(1024 * 1024 * 1024), "1.0 GB");
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
}
