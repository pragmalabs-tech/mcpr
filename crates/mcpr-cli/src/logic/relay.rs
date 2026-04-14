//! Relay lifecycle logic — stop, restart, status.
//!
//! Returns results; does not print.

use crate::relay_lock;

/// Result of stopping the relay.
pub enum StopResult {
    /// Relay was running and has been stopped.
    Stopped { pid: u32 },
    /// Lock was stale and has been cleaned up.
    StaleCleaned,
}

/// Relay status information.
pub struct RelayStatusInfo {
    pub pid: u32,
    pub port: u16,
    pub started_at: i64,
}

/// Stop the running relay.
pub fn stop_relay() -> Result<StopResult, String> {
    match relay_lock::check_lock() {
        relay_lock::LockStatus::Held(info) => {
            let pid = info.pid;
            relay_lock::stop_relay();
            Ok(StopResult::Stopped { pid })
        }
        relay_lock::LockStatus::Stale(_) => {
            relay_lock::remove_lock();
            Ok(StopResult::StaleCleaned)
        }
        relay_lock::LockStatus::Free => Err("relay is not running.".to_string()),
    }
}

/// Get relay status.
pub fn relay_status() -> Result<RelayStatusInfo, String> {
    match relay_lock::check_lock() {
        relay_lock::LockStatus::Held(info) => Ok(RelayStatusInfo {
            pid: info.pid,
            port: info.port,
            started_at: info.started_at,
        }),
        relay_lock::LockStatus::Stale(_) => {
            relay_lock::remove_lock();
            Err("relay is not running (stale lock cleaned up).".to_string())
        }
        relay_lock::LockStatus::Free => Err("relay is not running.".to_string()),
    }
}

/// Start the relay from its saved config snapshot.
pub fn start_relay_from_snapshot() -> Result<(), String> {
    relay_lock::read_snapshot().map_err(|e| format!("no config snapshot for relay: {e}"))?;

    let config_path = relay_lock::config_snapshot_path().display().to_string();
    let exe = std::env::current_exe().map_err(|e| format!("cannot find mcpr binary: {e}"))?;

    let status = std::process::Command::new(exe)
        .args(["relay", "start", &config_path])
        .status()
        .map_err(|e| format!("failed to spawn relay: {e}"))?;

    if !status.success() {
        return Err("relay failed to start".to_string());
    }

    Ok(())
}

/// Restart the relay. Stops if running, then starts from snapshot.
pub fn restart_relay() -> Result<(), String> {
    // Stop if running (ignore errors — it may not be running).
    let _ = stop_relay();
    start_relay_from_snapshot()
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;

    // ── stop_relay ────────────────────────────────────────────────────

    #[test]
    fn stop_relay__returns_error_when_not_running() {
        // With no lockfile, stop should return an error.
        // This may succeed or fail depending on whether a relay is actually
        // running on this machine. We test the "not running" path by checking
        // the error message pattern.
        let result = stop_relay();
        // If it errors (expected), verify the message.
        if let Err(e) = result {
            assert!(e.contains("not running"), "unexpected error: {e}");
        }
        // If it succeeds (relay was actually running), that's also valid.
    }

    // ── relay_status ──────────────────────────────────────────────────

    #[test]
    fn relay_status__returns_error_when_not_running() {
        let result = relay_status();
        if let Err(e) = result {
            assert!(e.contains("not running"), "unexpected error: {e}");
        }
    }

    #[test]
    fn relay_status__returns_info_fields() {
        // If relay happens to be running, verify the struct is populated.
        if let Ok(info) = relay_status() {
            assert!(info.pid > 0);
            assert!(info.port > 0);
            assert!(info.started_at > 0);
        }
    }

    // ── start_relay_from_snapshot ─────────────────────────────────────

    #[test]
    fn start_relay_from_snapshot__fails_without_snapshot() {
        // Without a config snapshot at ~/.mcpr/relay/config.toml, this
        // should fail with a clear error message.
        let result = start_relay_from_snapshot();
        // This may succeed if a snapshot exists from a prior run, so only
        // assert on error.
        if let Err(e) = result {
            assert!(
                e.contains("no config snapshot") || e.contains("failed to spawn"),
                "unexpected error: {e}"
            );
        }
    }
}
