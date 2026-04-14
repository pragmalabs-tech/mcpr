//! Relay lifecycle logic — stop, restart, status.
//!
//! Returns results; does not print.

use crate::relay_lock;

/// Result of stopping the relay.
#[derive(Debug)]
pub enum StopResult {
    /// Relay was running and has been stopped.
    Stopped { pid: u32 },
    /// Lock was stale and has been cleaned up.
    StaleCleaned,
}

/// Relay status information.
#[derive(Debug)]
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

    // Tests that touch the real ~/.mcpr/relay/ lockfile must run
    // sequentially to avoid parallel conflicts. Combined into one test.

    #[cfg(unix)]
    #[test]
    fn lifecycle__all_paths() {
        // 1. Free → stop returns error.
        relay_lock::remove_lock();
        let result = stop_relay();
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not running"));

        // 2. Free → status returns error.
        let result = relay_status();
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not running"));

        // 3. Held (own PID) → status returns info.
        relay_lock::write_lock(7777, "/tmp/test-relay.toml").unwrap();
        let result = relay_status();
        assert!(result.is_ok());
        let info = result.unwrap();
        assert_eq!(info.pid, std::process::id());
        assert_eq!(info.port, 7777);
        assert!(info.started_at > 0);
        relay_lock::remove_lock();

        // 4. Stale (dead PID) → status cleans up and returns error.
        let dir = relay_lock::config_snapshot_path()
            .parent()
            .unwrap()
            .to_path_buf();
        std::fs::create_dir_all(&dir).unwrap();
        let ts = chrono::Utc::now().timestamp();
        std::fs::write(
            dir.join("lock"),
            format!("99999999\n8080\n{ts}\n/tmp/relay.toml\n"),
        )
        .unwrap();
        let result = relay_status();
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("stale"));
        assert!(relay_lock::read_lock_info().is_none());

        // 5. Stale → stop returns StaleCleaned.
        std::fs::write(
            dir.join("lock"),
            format!("99999999\n8080\n{ts}\n/tmp/relay.toml\n"),
        )
        .unwrap();
        let result = stop_relay();
        assert!(result.is_ok());
        assert!(matches!(result.unwrap(), StopResult::StaleCleaned));
        assert!(relay_lock::read_lock_info().is_none());
    }

    #[test]
    fn start_relay_from_snapshot__fails_without_snapshot() {
        let _ = std::fs::remove_file(relay_lock::config_snapshot_path());
        let result = start_relay_from_snapshot();
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("no config snapshot"));
    }
}
