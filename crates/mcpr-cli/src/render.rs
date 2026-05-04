//! Terminal rendering layer.
//!
//! Every function in this module receives **data** and writes it to
//! stdout/stderr.  No database access, no process management — just
//! formatting and printing.  Commands call into this module after
//! obtaining data from `logic::*`.

use std::path::Path;

use mcpr_integrations::store::query::store_ops::{StoreStats, VacuumResult};

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
