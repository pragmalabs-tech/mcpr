//! Time formatting utilities shared across mcpr crates.

/// Format a latency value in microseconds for human-readable display.
///
/// Three display tiers:
/// - **< 1ms** (< 1,000μs): `"142μs"`
/// - **1ms..1s** (1,000..1,000,000μs): `"4.20ms"` (two decimal places)
/// - **≥ 1s** (≥ 1,000,000μs): `"1,234ms"` (comma-separated, no decimals)
///
/// # Examples
///
/// ```
/// use mcpr_core::utils::time::format_latency_us;
///
/// assert_eq!(format_latency_us(200), "200μs");
/// assert_eq!(format_latency_us(4_200), "4.20ms");
/// assert_eq!(format_latency_us(1_500_000), "1,500ms");
/// ```
pub fn format_latency_us(us: i64) -> String {
    if us < 1_000 {
        format!("{us}μs")
    } else {
        let ms = us as f64 / 1_000.0;
        if ms >= 1_000.0 {
            let ms_int = (ms + 0.5) as i64;
            format!("{},{:03}ms", ms_int / 1000, ms_int % 1000)
        } else {
            format!("{ms:.2}ms")
        }
    }
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;

    #[test]
    fn format_latency_us__sub_ms() {
        assert_eq!(format_latency_us(0), "0μs");
        assert_eq!(format_latency_us(1), "1μs");
        assert_eq!(format_latency_us(200), "200μs");
        assert_eq!(format_latency_us(999), "999μs");
    }

    #[test]
    fn format_latency_us__ms_range() {
        assert_eq!(format_latency_us(1_000), "1.00ms");
        assert_eq!(format_latency_us(1_500), "1.50ms");
        assert_eq!(format_latency_us(4_200), "4.20ms");
        assert_eq!(format_latency_us(10_250), "10.25ms");
        assert_eq!(format_latency_us(142_000), "142.00ms");
        assert_eq!(format_latency_us(500_000), "500.00ms");
    }

    #[test]
    fn format_latency_us__seconds_range() {
        assert_eq!(format_latency_us(1_000_000), "1,000ms");
        assert_eq!(format_latency_us(1_500_000), "1,500ms");
        assert_eq!(format_latency_us(4_201_000), "4,201ms");
        assert_eq!(format_latency_us(12_345_000), "12,345ms");
    }

    #[test]
    fn format_latency_us__boundary_us_to_ms() {
        assert_eq!(format_latency_us(999), "999μs");
        assert_eq!(format_latency_us(1_000), "1.00ms");
    }

    #[test]
    fn format_latency_us__boundary_ms_to_s() {
        assert_eq!(format_latency_us(999_999), "1000.00ms");
        assert_eq!(format_latency_us(1_000_000), "1,000ms");
    }
}
