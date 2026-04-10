//! CLI command handlers for proxy queries and store operations.
//!
//! These are read-only commands that open the SQLite store directly —
//! no running proxy needed. They query the database, format the output,
//! and exit.

use mcpr_integrations::store::{
    self,
    query::{
        QueryEngine, clients::ClientsParams, logs::LogsParams, sessions::SessionsParams,
        slow::SlowParams, stats::StatsParams, store_ops::VacuumParams,
    },
};

use crate::config::*;
#[cfg(unix)]
use crate::daemon;

/// Resolve the proxy name — use the provided name, or auto-detect from the
/// running daemon's PID file (since we only have one proxy right now).
fn resolve_proxy_name(name: Option<String>) -> Result<String, String> {
    if let Some(n) = name {
        return Ok(n);
    }

    #[cfg(unix)]
    if let Some(info) = daemon::read_pid_file() {
        return Ok(info.proxy_name);
    }

    Err("proxy name required — pass it as an argument, or start the daemon first".to_string())
}

/// Resolve the store database path and open a query engine.
fn open_query_engine() -> Result<(QueryEngine, std::path::PathBuf), String> {
    let db_path = store::path::resolve_db_path(None)
        .ok_or_else(|| "could not determine store path — is $HOME set?".to_string())?;

    if !db_path.exists() {
        return Err(format!(
            "no store found at {} — has mcpr been run yet?",
            db_path.display()
        ));
    }

    let engine = QueryEngine::open(&db_path).map_err(|e| format!("failed to open store: {e}"))?;
    Ok((engine, db_path))
}

/// Parse a --since or --before duration string to a unix ms cutoff timestamp.
fn parse_since(s: &str) -> Result<i64, String> {
    let dur = store::parse_duration(s)
        .ok_or_else(|| format!("invalid duration: {s} (expected: 30m, 1h, 7d, etc.)"))?;
    Ok(store::since_to_cutoff_ms(dur))
}

/// Parse a --threshold duration string to milliseconds.
fn parse_threshold_ms(s: &str) -> Result<i64, String> {
    if let Some(ms_str) = s.strip_suffix("ms") {
        return ms_str
            .trim()
            .parse::<i64>()
            .map_err(|_| format!("invalid threshold: {s}"));
    }
    let dur = store::parse_duration(s)
        .ok_or_else(|| format!("invalid threshold: {s} (expected: 500ms, 1s, etc.)"))?;
    Ok(dur.as_millis() as i64)
}

// ── Formatting helpers ─────────────────────────────────────────────────

/// Format a unix ms timestamp as a human-readable local time.
fn format_ts(ts: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ts)
        .map(|dt| {
            dt.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(|| "?".to_string())
}

/// Format bytes as a human-readable size.
fn format_bytes(bytes: u64) -> String {
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

/// Format latency with comma separators for readability.
fn format_latency(ms: i64) -> String {
    if ms >= 1000 {
        format!("{},{:03}ms", ms / 1000, ms % 1000)
    } else {
        format!("{ms}ms")
    }
}

/// Print a serializable struct as a single JSON line.
fn print_json(value: &impl serde::Serialize) {
    if let Ok(json) = serde_json::to_string(value) {
        println!("{json}");
    }
}

// ── Proxy commands ─────────────────────────────────────────────────────

pub fn handle_proxy_command(cmd: ProxyCommand) {
    let result = match cmd {
        ProxyCommand::Logs(args) => cmd_proxy_logs(args),
        ProxyCommand::Slow(args) => cmd_proxy_slow(args),
        ProxyCommand::Stats(args) => cmd_proxy_stats(args),
        ProxyCommand::Sessions(args) => cmd_proxy_sessions(args),
        ProxyCommand::Clients(args) => cmd_proxy_clients(args),
    };

    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn cmd_proxy_logs(args: ProxyLogsArgs) -> Result<(), String> {
    let (engine, _) = open_query_engine()?;
    let name = resolve_proxy_name(args.name)?;
    let since_ts = parse_since(&args.since)?;

    let params = LogsParams {
        proxy: name,
        since_ts,
        limit: args.tail,
        tool: args.tool.clone(),
        status: args.status.clone(),
    };

    let rows = engine
        .logs(&params)
        .map_err(|e| format!("query failed: {e}"))?;

    if args.json {
        for row in &rows {
            print_json(row);
        }
    } else {
        println!(
            "{:<21} {:<14} {:<22} {:>8}  STATUS",
            "TIME", "METHOD", "TOOL", "LATENCY"
        );
        for row in &rows {
            let tool = row.tool.as_deref().unwrap_or("—");
            let status_str = match row.status.as_str() {
                "error" => format!("error  {:?}", row.error_msg.as_deref().unwrap_or("")),
                s => s.to_string(),
            };
            println!(
                "{:<21} {:<14} {:<22} {:>8}  {}",
                format_ts(row.ts),
                row.method,
                tool,
                format_latency(row.latency_ms),
                status_str,
            );
        }
        if rows.is_empty() {
            println!("  (no records found)");
        }
    }

    // --follow mode: poll for new rows
    if args.follow {
        let mut last_ts = rows.last().map(|r| r.ts).unwrap_or(since_ts);
        loop {
            std::thread::sleep(std::time::Duration::from_millis(500));
            let new_rows = engine
                .logs_since(&params, last_ts)
                .map_err(|e| format!("follow query failed: {e}"))?;
            for row in &new_rows {
                if args.json {
                    print_json(row);
                } else {
                    let tool = row.tool.as_deref().unwrap_or("—");
                    println!(
                        "{:<21} {:<14} {:<22} {:>8}  {}",
                        format_ts(row.ts),
                        row.method,
                        tool,
                        format_latency(row.latency_ms),
                        row.status,
                    );
                }
                last_ts = row.ts;
            }
        }
    }

    Ok(())
}

fn cmd_proxy_slow(args: ProxySlowArgs) -> Result<(), String> {
    let (engine, _) = open_query_engine()?;
    let name = resolve_proxy_name(args.name)?;
    let since_ts = parse_since(&args.since)?;
    let threshold_ms = parse_threshold_ms(&args.threshold)?;

    let rows = engine
        .slow(&SlowParams {
            proxy: name.clone(),
            threshold_ms,
            since_ts,
            limit: args.limit,
        })
        .map_err(|e| format!("query failed: {e}"))?;

    if args.json {
        for row in &rows {
            print_json(row);
        }
    } else {
        println!(
            "TOP SLOW CALLS — {} — last {} (threshold: {})\n",
            name, args.since, args.threshold
        );
        println!(
            "  {:<22} {:>10}   {:<21}  STATUS",
            "TOOL", "LATENCY", "TIME"
        );
        for row in &rows {
            let tool = row.tool.as_deref().unwrap_or(&row.method);
            println!(
                "  {:<22} {:>10}   {:<21}  {}",
                tool,
                format_latency(row.latency_ms),
                format_ts(row.ts),
                row.status,
            );
        }
        if rows.is_empty() {
            println!("  (no slow calls found)");
        } else {
            let avg: i64 = rows.iter().map(|r| r.latency_ms).sum::<i64>() / rows.len() as i64;
            println!(
                "\n  {} calls above threshold in last {} (avg: {})",
                rows.len(),
                args.since,
                format_latency(avg),
            );
        }
    }

    Ok(())
}

fn cmd_proxy_stats(args: ProxyStatsArgs) -> Result<(), String> {
    let (engine, _) = open_query_engine()?;
    let name = resolve_proxy_name(args.name)?;
    let since_ts = parse_since(&args.since)?;

    let result = engine
        .stats(&StatsParams {
            proxy: name.clone(),
            since_ts,
        })
        .map_err(|e| format!("query failed: {e}"))?;

    if args.json {
        print_json(&result);
    } else {
        println!(
            "STATS — {} — last {}   Total: {} calls   Errors: {:.1}%\n",
            name, args.since, result.total_calls, result.error_pct
        );
        println!(
            "  {:<22} {:>6}  {:>7}  {:>7}  {:>7}  {:>8}",
            "TOOL", "CALLS", "AVG", "P95", "MAX", "ERRORS"
        );
        for t in &result.tools {
            let error_str = if t.error_pct > 0.0 {
                format!("{:.1}%", t.error_pct)
            } else {
                "0%".to_string()
            };
            println!(
                "  {:<22} {:>6}  {:>7}  {:>7}  {:>7}  {:>8}",
                t.label,
                t.calls,
                format_latency(t.avg_ms as i64),
                format_latency(t.p95_ms),
                format_latency(t.max_ms),
                error_str,
            );
        }
        if result.tools.is_empty() {
            println!("  (no data)");
        }
    }

    Ok(())
}

fn cmd_proxy_sessions(args: ProxySessionsArgs) -> Result<(), String> {
    let (engine, _) = open_query_engine()?;
    let name = resolve_proxy_name(args.name)?;
    let since_ts = parse_since(&args.since)?;

    let rows = engine
        .sessions(&SessionsParams {
            proxy: name.clone(),
            since_ts,
            limit: args.limit,
            active_only: args.active,
            client: args.client.clone(),
        })
        .map_err(|e| format!("query failed: {e}"))?;

    if args.json {
        for row in &rows {
            print_json(row);
        }
    } else {
        println!("SESSIONS — {} — last {}\n", name, args.since);
        println!(
            "  {:<16} {:<24} {:<17} {:>12} {:>6} {:>6}",
            "SESSION", "CLIENT", "STARTED", "LAST SEEN", "CALLS", "ERRS"
        );
        for row in &rows {
            let client = match (&row.client_name, &row.client_version) {
                (Some(n), Some(v)) => format!("{n} {v}"),
                (Some(n), None) => n.clone(),
                _ => "unknown".to_string(),
            };
            let status_icon = if row.is_active { "●" } else { "○" };
            let sid = if row.session_id.len() > 14 {
                format!("{}…", &row.session_id[..14])
            } else {
                row.session_id.clone()
            };
            println!(
                "  {:<16} {:<24} {:<17} {:>12} {:>6} {:>6}",
                sid,
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

    Ok(())
}

fn cmd_proxy_clients(args: ProxyClientsArgs) -> Result<(), String> {
    let (engine, _) = open_query_engine()?;
    let name = resolve_proxy_name(args.name)?;
    let since_ts = parse_since(&args.since)?;

    let rows = engine
        .clients(&ClientsParams {
            proxy: name.clone(),
            since_ts,
        })
        .map_err(|e| format!("query failed: {e}"))?;

    if args.json {
        for row in &rows {
            print_json(row);
        }
    } else {
        println!("CLIENTS — {} — last {}\n", name, args.since);
        println!(
            "  {:<20} {:<10} {:<10} {:>8} {:>8} {:>8}",
            "CLIENT", "VERSION", "PLATFORM", "SESSIONS", "CALLS", "ERRORS"
        );
        for row in &rows {
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

    Ok(())
}

// ── Store commands ─────────────────────────────────────────────────────

pub fn handle_store_command(cmd: StoreCommand) {
    let result = match cmd {
        StoreCommand::Stats => cmd_store_stats(),
        StoreCommand::Vacuum(args) => cmd_store_vacuum(args),
    };

    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn cmd_store_stats() -> Result<(), String> {
    let (engine, db_path) = open_query_engine()?;

    let stats = engine
        .store_stats(&db_path)
        .map_err(|e| format!("query failed: {e}"))?;

    println!("STORAGE — {}\n", db_path.display());
    println!("  Total requests:    {}", stats.total_requests);
    println!("  Total sessions:    {}", stats.total_sessions);
    println!("  Proxies tracked:   {}", stats.proxy_count);
    if let Some(oldest) = stats.oldest_ts {
        println!("  Oldest record:     {}", format_ts(oldest));
    }
    if let Some(newest) = stats.newest_ts {
        println!("  Newest record:     {}", format_ts(newest));
    }
    println!();
    println!("  Database file:     {}", format_bytes(stats.db_file_size));
    println!("  WAL file:          {}", format_bytes(stats.wal_file_size));

    if stats.db_file_size > 500 * 1024 * 1024 {
        println!("\n  Run `mcpr store vacuum --before 7d` to remove records older than 7 days.");
    }

    Ok(())
}

fn cmd_store_vacuum(args: StoreVacuumArgs) -> Result<(), String> {
    let (engine, _) = open_query_engine()?;
    let before_ts = parse_since(&args.before)?;

    let result = engine
        .vacuum(&VacuumParams {
            before_ts,
            proxy: args.proxy.clone(),
            dry_run: args.dry_run,
        })
        .map_err(|e| format!("vacuum failed: {e}"))?;

    if args.dry_run {
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

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_since_valid() {
        let ts = parse_since("1h").unwrap();
        let now = chrono::Utc::now().timestamp_millis();
        // Should be roughly 1 hour ago (within 1 second tolerance)
        assert!((now - ts - 3_600_000).abs() < 1000);
    }

    #[test]
    fn parse_since_invalid() {
        assert!(parse_since("bad").is_err());
        assert!(parse_since("").is_err());
    }

    #[test]
    fn parse_threshold_ms_millis() {
        assert_eq!(parse_threshold_ms("500ms").unwrap(), 500);
        assert_eq!(parse_threshold_ms("100ms").unwrap(), 100);
    }

    #[test]
    fn parse_threshold_ms_seconds() {
        assert_eq!(parse_threshold_ms("1s").unwrap(), 1000);
        assert_eq!(parse_threshold_ms("2s").unwrap(), 2000);
    }

    #[test]
    fn parse_threshold_ms_invalid() {
        assert!(parse_threshold_ms("bad").is_err());
        assert!(parse_threshold_ms("ms").is_err());
    }

    #[test]
    fn format_latency_under_1s() {
        assert_eq!(format_latency(142), "142ms");
        assert_eq!(format_latency(0), "0ms");
        assert_eq!(format_latency(999), "999ms");
    }

    #[test]
    fn format_latency_over_1s() {
        assert_eq!(format_latency(1000), "1,000ms");
        assert_eq!(format_latency(4201), "4,201ms");
        assert_eq!(format_latency(12345), "12,345ms");
    }

    #[test]
    fn format_bytes_units() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1024), "1.0 KB");
        assert_eq!(format_bytes(1024 * 1024), "1.0 MB");
        assert_eq!(format_bytes(1024 * 1024 * 1024), "1.0 GB");
    }

    #[test]
    fn format_ts_valid() {
        let ts = 1712345678000_i64; // 2024-04-05T18:34:38Z
        let result = format_ts(ts);
        // Should be a valid date string (not "?")
        assert_ne!(result, "?");
        assert!(result.contains("2024"));
    }

    #[test]
    fn format_ts_zero() {
        let result = format_ts(0);
        assert_ne!(result, "?"); // epoch is valid
    }
}
