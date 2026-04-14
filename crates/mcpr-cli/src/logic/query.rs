//! Database query helpers — open the store, parse time/threshold args.
//!
//! Pure data operations.  No printing.

use mcpr_integrations::store::{self, query::QueryEngine};

/// Resolve the store database path and open a query engine.
pub fn open_query_engine() -> Result<(QueryEngine, std::path::PathBuf), String> {
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
pub fn parse_since(s: &str) -> Result<i64, String> {
    let dur = store::parse_duration(s)
        .ok_or_else(|| format!("invalid duration: {s} (expected: 30m, 1h, 7d, etc.)"))?;
    Ok(store::since_to_cutoff_ms(dur))
}

/// Parse a --threshold duration string to microseconds.
///
/// Accepts human-friendly units (500ms, 1s, 200us) and converts to μs.
pub fn parse_threshold_us(s: &str) -> Result<i64, String> {
    if let Some(us_str) = s.strip_suffix("us").or_else(|| s.strip_suffix("μs")) {
        return us_str
            .trim()
            .parse::<i64>()
            .map_err(|_| format!("invalid threshold: {s}"));
    }
    if let Some(ms_str) = s.strip_suffix("ms") {
        return ms_str
            .trim()
            .parse::<i64>()
            .map(|ms| ms * 1_000)
            .map_err(|_| format!("invalid threshold: {s}"));
    }
    let dur = store::parse_duration(s)
        .ok_or_else(|| format!("invalid threshold: {s} (expected: 500ms, 1s, 200us, etc.)"))?;
    Ok(dur.as_micros() as i64)
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;

    #[test]
    fn parse_since__valid() {
        let ts = parse_since("1h").unwrap();
        let now = chrono::Utc::now().timestamp_millis();
        assert!((now - ts - 3_600_000).abs() < 1000);
    }

    #[test]
    fn parse_since__invalid() {
        assert!(parse_since("bad").is_err());
        assert!(parse_since("").is_err());
    }

    #[test]
    fn parse_threshold_us__micros() {
        assert_eq!(parse_threshold_us("200us").unwrap(), 200);
        assert_eq!(parse_threshold_us("500μs").unwrap(), 500);
    }

    #[test]
    fn parse_threshold_us__millis() {
        assert_eq!(parse_threshold_us("500ms").unwrap(), 500_000);
        assert_eq!(parse_threshold_us("100ms").unwrap(), 100_000);
    }

    #[test]
    fn parse_threshold_us__seconds() {
        assert_eq!(parse_threshold_us("1s").unwrap(), 1_000_000);
        assert_eq!(parse_threshold_us("2s").unwrap(), 2_000_000);
    }

    #[test]
    fn parse_threshold_us__invalid() {
        assert!(parse_threshold_us("bad").is_err());
        assert!(parse_threshold_us("ms").is_err());
    }

    #[test]
    fn parse_threshold_us__zero() {
        assert_eq!(parse_threshold_us("0us").unwrap(), 0);
        assert_eq!(parse_threshold_us("0ms").unwrap(), 0);
    }

    #[test]
    fn parse_threshold_us__large_values() {
        assert_eq!(parse_threshold_us("10s").unwrap(), 10_000_000);
        assert_eq!(parse_threshold_us("5000ms").unwrap(), 5_000_000);
    }

    #[test]
    fn parse_threshold_us__rejects_empty_number() {
        assert!(parse_threshold_us("us").is_err());
        assert!(parse_threshold_us("μs").is_err());
    }
}
