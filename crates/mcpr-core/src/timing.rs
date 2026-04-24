//! Per-stage wall-clock timing instrumentation.
//!
//! Gated on the `MCPR_STAGE_TIMING` env var, checked **once** per
//! process and cached via `OnceLock`. When disabled,
//! [`StageTimer::mark`] is a no-op and [`StageTimer::finish`] returns
//! `None` — ~1 ns of overhead from a cached branch predict.
//!
//! When enabled, timings populate [`crate::event::StageTimings`] which
//! handlers attach to each `RequestEvent`. Events flow through the
//! existing event bus, so sinks (stderr JSON, sqlite, cloud) pick
//! them up without extra wiring.
//!
//! ## Enabling
//!
//! ```text
//! MCPR_STAGE_TIMING=1 mcpr proxy run ./mcpr.toml
//! # or
//! MCPR_STAGE_TIMING=true mcpr proxy run ...
//! ```
//!
//! ## Reading the data
//!
//! The JSON stderr sink (default log format) writes each event —
//! including `stage_timings` — as one JSON line to the proxy's log
//! file at `~/.mcpr/proxies/<name>/proxy.log`. To aggregate:
//!
//! ```text
//! tail -n +N ~/.mcpr/proxies/bench/proxy.log \
//!     | jq -c 'select(.stage_timings)' \
//!     | jq -s '[.[].stage_timings.schema_us // empty] | sort'
//! ```
//!
//! The `benches/scripts/scenarios/where-time-goes.sh` harness does
//! this aggregation automatically and prints a per-stage summary.

use std::sync::OnceLock;
use std::time::Instant;

use crate::event::StageTimings;

const MCPR_STAGE_TIMING_ENV: &str = "MCPR_STAGE_TIMING";

/// Check the env var once and cache. Subsequent calls are a single
/// atomic load (~1 ns). Returns `true` when the env var is set to
/// `"1"`, `"true"`, or `"yes"` (case-sensitive — keep it strict so
/// typos don't accidentally enable instrumentation in production).
fn timing_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        matches!(
            std::env::var(MCPR_STAGE_TIMING_ENV).as_deref(),
            Ok("1") | Ok("true") | Ok("yes")
        )
    })
}

/// Named stages the pipeline reports timings for. Keep this list
/// aligned with the `Option<u32>` fields on
/// [`crate::event::StageTimings`].
#[derive(Clone, Copy, Debug)]
pub enum Stage {
    /// Buffering the upstream response body (`read_body_capped`).
    Buffer,
    /// SSE frame extraction on the response body.
    SseUnwrap,
    /// JSON parse of the buffered/unwrapped body.
    JsonParse,
    /// `SchemaManager::ingest` + stale-flag check.
    Schema,
    /// Marker scan (`rewrite::has_markers`).
    MarkerScan,
    /// Structured CSP rewrite (`rewrite::rewrite_in_place`).
    Rewrite,
    /// `serde_json::to_vec` when reserialization was needed.
    Reserialize,
    /// Passthrough URL substitution (non-MCP JSON path).
    UrlMap,
    /// Post-response side-effects (session start, health, client info).
    SideEffects,
}

/// Lightweight per-request stopwatch. Creation and every `mark()` are
/// no-ops when [`MCPR_STAGE_TIMING`](const@MCPR_STAGE_TIMING_ENV) is
/// not set — safe to sprinkle liberally through hot paths.
pub struct StageTimer {
    state: State,
}

enum State {
    /// Env var unset — all ops are no-ops.
    Disabled,
    /// Env var set — track the clock.
    Enabled {
        last: Instant,
        timings: StageTimings,
    },
}

impl Default for StageTimer {
    fn default() -> Self {
        Self::new()
    }
}

impl StageTimer {
    /// Construct. Cheap either way — ~1 ns disabled, one `Instant::now()`
    /// (~10 ns) when enabled.
    pub fn new() -> Self {
        let state = if timing_enabled() {
            State::Enabled {
                last: Instant::now(),
                timings: StageTimings::default(),
            }
        } else {
            State::Disabled
        };
        Self { state }
    }

    /// Record the microseconds elapsed since the previous mark (or
    /// construction) into `stage`'s slot, then reset the clock.
    /// No-op when disabled.
    pub fn mark(&mut self, stage: Stage) {
        let State::Enabled { last, timings } = &mut self.state else {
            return;
        };
        let us = last.elapsed().as_micros() as u32;
        match stage {
            Stage::Buffer => timings.buffer_us = Some(us),
            Stage::SseUnwrap => timings.sse_unwrap_us = Some(us),
            Stage::JsonParse => timings.json_parse_us = Some(us),
            Stage::Schema => timings.schema_us = Some(us),
            Stage::MarkerScan => timings.marker_scan_us = Some(us),
            Stage::Rewrite => timings.rewrite_us = Some(us),
            Stage::Reserialize => timings.reserialize_us = Some(us),
            Stage::UrlMap => timings.url_map_us = Some(us),
            Stage::SideEffects => timings.side_effects_us = Some(us),
        }
        *last = Instant::now();
    }

    /// Consume the timer and return the accumulated timings. Returns
    /// `None` when disabled — callers assign directly into
    /// `Working::timings` so `None` means "don't emit this field."
    pub fn finish(self) -> Option<StageTimings> {
        match self.state {
            State::Enabled { timings, .. } => Some(timings),
            State::Disabled => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Note: `timing_enabled()` reads env ONCE via OnceLock, so these
    // tests can't toggle it mid-process. They exercise the structural
    // invariants instead: disabled timer is cheap and always returns
    // None; enabled timer (forced via direct construction) records.

    #[test]
    fn disabled_timer_is_noop() {
        let mut t = StageTimer {
            state: State::Disabled,
        };
        t.mark(Stage::Schema); // must not panic / allocate
        assert!(t.finish().is_none());
    }

    #[test]
    fn enabled_timer_records_marks() {
        let mut t = StageTimer {
            state: State::Enabled {
                last: Instant::now(),
                timings: StageTimings::default(),
            },
        };
        // Sleep a tiny bit so the microsecond field is non-zero.
        std::thread::sleep(std::time::Duration::from_micros(50));
        t.mark(Stage::Schema);
        let out = t.finish().expect("enabled timer should yield Some");
        assert!(out.schema_us.is_some(), "schema_us should be recorded");
        assert!(
            out.json_parse_us.is_none(),
            "only marked stages should be populated"
        );
    }
}
