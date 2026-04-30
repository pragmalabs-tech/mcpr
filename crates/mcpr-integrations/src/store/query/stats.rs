//! Query: per-tool aggregated metrics (used by `mcpr proxy status`).

use rusqlite::params;
use serde::Serialize;

use super::QueryEngine;

/// Raw aggregate row from SQL: (label, calls, avg_us, min_us, max_us, error_pct, bytes_in, bytes_out).
type AggRow = (String, i64, f64, i64, i64, f64, i64, i64);

/// Filter parameters for the stats query.
pub struct StatsParams {
    /// Proxy name to filter by (None = all proxies).
    pub proxy: Option<String>,
    /// Only rows newer than this unix ms timestamp.
    pub since_ts: i64,
}

/// Aggregated stats for one tool (or method).
#[derive(Debug, Clone, Serialize)]
pub struct ToolStats {
    /// Tool name, or `<method>` for non-tool-call methods.
    pub label: String,
    /// Total number of calls.
    pub calls: i64,
    /// Average latency in microseconds.
    pub avg_us: f64,
    /// Minimum latency in microseconds.
    pub min_us: i64,
    /// Maximum latency in microseconds.
    pub max_us: i64,
    /// 95th percentile latency in microseconds (approximate).
    pub p95_us: i64,
    /// Error percentage (0.0 to 100.0).
    pub error_pct: f64,
    /// Total request bytes.
    pub total_bytes_in: i64,
    /// Total response bytes.
    pub total_bytes_out: i64,
}

/// Aggregated result for the stats command.
#[derive(Debug, Serialize)]
pub struct StatsResult {
    /// Per-tool/method breakdown, sorted by call count descending.
    pub tools: Vec<ToolStats>,
    /// Total calls across all tools.
    pub total_calls: i64,
    /// Overall error percentage.
    pub error_pct: f64,
}

impl QueryEngine {
    /// Compute per-tool aggregated stats for a proxy within a time window.
    ///
    /// Percentiles are computed in Rust (load latency values into a Vec and sort)
    /// because SQLite has no native percentile function. This is fine for the
    /// expected data volumes (<1M rows per proxy).
    pub fn stats(&self, params: &StatsParams) -> Result<StatsResult, rusqlite::Error> {
        // Step 1: Get basic aggregates per tool/method group.
        let agg_sql = "
            SELECT
                COALESCE(tool, '<' || method || '>') AS label,
                COUNT(*) AS calls,
                AVG(latency_us) AS avg_us,
                MIN(latency_us) AS min_us,
                MAX(latency_us) AS max_us,
                SUM(CASE WHEN status != 'ok' THEN 1 ELSE 0 END) * 100.0
                    / COUNT(*) AS error_pct,
                COALESCE(SUM(bytes_in), 0) AS total_bytes_in,
                COALESCE(SUM(bytes_out), 0) AS total_bytes_out
            FROM request_log
            WHERE (?1 IS NULL OR proxy = ?1) AND ts >= ?2
            GROUP BY COALESCE(tool, '<' || method || '>')
            ORDER BY calls DESC
        ";

        let mut stmt = self.conn().prepare(agg_sql)?;
        let groups: Vec<AggRow> = stmt
            .query_map(params![params.proxy, params.since_ts], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;

        // Step 2: Compute p95 per group by loading latency values into Rust.
        let p95_sql = "
            SELECT latency_us
            FROM request_log
            WHERE (?1 IS NULL OR proxy = ?1) AND ts >= ?2
              AND COALESCE(tool, '<' || method || '>') = ?3
            ORDER BY latency_us
        ";

        let mut total_calls: i64 = 0;
        let mut total_errors: f64 = 0.0;
        let mut tools = Vec::with_capacity(groups.len());

        for (label, calls, avg_us, min_us, max_us, error_pct, bytes_in, bytes_out) in &groups {
            // Load all latency values for this group to compute p95.
            let mut p95_stmt = self.conn().prepare(p95_sql)?;
            let latencies: Vec<i64> = p95_stmt
                .query_map(params![params.proxy, params.since_ts, label], |row| {
                    row.get(0)
                })?
                .collect::<Result<Vec<_>, _>>()?;

            let p95 = percentile(&latencies, 95);

            total_calls += calls;
            total_errors += (*calls as f64) * error_pct / 100.0;

            tools.push(ToolStats {
                label: label.clone(),
                calls: *calls,
                avg_us: *avg_us,
                min_us: *min_us,
                max_us: *max_us,
                p95_us: p95,
                error_pct: *error_pct,
                total_bytes_in: *bytes_in,
                total_bytes_out: *bytes_out,
            });
        }

        let overall_error_pct = if total_calls > 0 {
            total_errors / total_calls as f64 * 100.0
        } else {
            0.0
        };

        Ok(StatsResult {
            tools,
            total_calls,
            error_pct: overall_error_pct,
        })
    }
}

/// Compute the Nth percentile from a sorted (ascending) list of values.
///
/// Uses nearest-rank method. Returns 0 for empty input.
fn percentile(sorted_values: &[i64], pct: u8) -> i64 {
    if sorted_values.is_empty() {
        return 0;
    }
    let idx = ((pct as f64 / 100.0) * sorted_values.len() as f64).ceil() as usize;
    let idx = idx.min(sorted_values.len()) - 1;
    sorted_values[idx]
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;

    #[test]
    fn percentile__basic() {
        let values = vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10];
        assert_eq!(percentile(&values, 50), 5);
        assert_eq!(percentile(&values, 95), 10);
        assert_eq!(percentile(&values, 100), 10);
    }

    #[test]
    fn percentile__empty() {
        assert_eq!(percentile(&[], 95), 0);
    }

    #[test]
    fn percentile__single() {
        assert_eq!(percentile(&[42], 95), 42);
    }
}
