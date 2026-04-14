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
