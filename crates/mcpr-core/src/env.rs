//! Process-wide environment toggles read from env vars.

use std::sync::OnceLock;

/// Read-once accessors for opt-in debug behavior. Evaluated the first
/// time a getter is called; later changes to the env are ignored.
pub struct Environment;

impl Environment {
    /// True when `MCPR_DEBUG` is set to `1` or `true` (case-insensitive).
    /// Gates verbose, runtime-only output like the per-request timer dump.
    pub fn debug() -> bool {
        static FLAG: OnceLock<bool> = OnceLock::new();
        *FLAG.get_or_init(|| {
            std::env::var("MCPR_DEBUG")
                .map(|v| {
                    let lower = v.to_ascii_lowercase();
                    lower == "1" || lower == "true"
                })
                .unwrap_or(false)
        })
    }
}
