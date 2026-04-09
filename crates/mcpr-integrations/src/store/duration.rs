//! Human-friendly duration parsing for `--since` and `--before` CLI flags.
//!
//! Supports formats like `30m`, `1h`, `24h`, `7d`, `2w`. Used across all
//! CLI observability commands to specify time windows.
//!
//! This is intentionally simple — no ISO 8601 duration parsing, no combined
//! units (e.g., "1h30m"). Each value is a single number + unit suffix.

use std::time::Duration;

/// Parse a human-friendly duration string into a [`Duration`].
///
/// Supported suffixes:
/// - `s` — seconds (e.g., `30s`)
/// - `m` — minutes (e.g., `15m`)
/// - `h` — hours (e.g., `2h`)
/// - `d` — days (e.g., `7d`)
/// - `w` — weeks (e.g., `2w`)
///
/// Returns `None` if the string is empty, has an unknown suffix, or the
/// numeric part can't be parsed.
///
/// # Examples
///
/// ```
/// use mcpr_integrations::store::duration::parse_duration;
///
/// assert_eq!(parse_duration("30m"), Some(std::time::Duration::from_secs(30 * 60)));
/// assert_eq!(parse_duration("2h"), Some(std::time::Duration::from_secs(2 * 3600)));
/// assert_eq!(parse_duration("7d"), Some(std::time::Duration::from_secs(7 * 86400)));
/// assert_eq!(parse_duration("bad"), None);
/// ```
pub fn parse_duration(s: &str) -> Option<Duration> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }

    // Split into numeric prefix and unit suffix.
    let (num_str, multiplier) = if let Some(n) = s.strip_suffix('w') {
        (n, 7 * 24 * 3600)
    } else if let Some(n) = s.strip_suffix('d') {
        (n, 24 * 3600)
    } else if let Some(n) = s.strip_suffix('h') {
        (n, 3600)
    } else if let Some(n) = s.strip_suffix('m') {
        (n, 60)
    } else if let Some(n) = s.strip_suffix('s') {
        (n, 1)
    } else {
        return None;
    };

    let num: u64 = num_str.trim().parse().ok()?;
    Some(Duration::from_secs(num * multiplier))
}

/// Convert a duration to a unix millisecond cutoff timestamp.
///
/// Returns `now_ms - duration_ms`, suitable for `WHERE ts >= ?` queries.
pub fn since_to_cutoff_ms(duration: Duration) -> i64 {
    let now_ms = chrono::Utc::now().timestamp_millis();
    now_ms - duration.as_millis() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_seconds() {
        assert_eq!(parse_duration("30s"), Some(Duration::from_secs(30)));
    }

    #[test]
    fn parse_minutes() {
        assert_eq!(parse_duration("15m"), Some(Duration::from_secs(15 * 60)));
    }

    #[test]
    fn parse_hours() {
        assert_eq!(parse_duration("2h"), Some(Duration::from_secs(2 * 3600)));
    }

    #[test]
    fn parse_days() {
        assert_eq!(parse_duration("7d"), Some(Duration::from_secs(7 * 86400)));
    }

    #[test]
    fn parse_weeks() {
        assert_eq!(
            parse_duration("2w"),
            Some(Duration::from_secs(2 * 7 * 86400))
        );
    }

    #[test]
    fn invalid_suffix() {
        assert_eq!(parse_duration("10x"), None);
    }

    #[test]
    fn invalid_number() {
        assert_eq!(parse_duration("abch"), None);
    }

    #[test]
    fn empty_string() {
        assert_eq!(parse_duration(""), None);
    }

    #[test]
    fn whitespace_handling() {
        assert_eq!(parse_duration(" 5m "), Some(Duration::from_secs(5 * 60)));
    }
}
