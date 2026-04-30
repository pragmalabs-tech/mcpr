//! Database query helpers — open the store, parse time args.
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
}
