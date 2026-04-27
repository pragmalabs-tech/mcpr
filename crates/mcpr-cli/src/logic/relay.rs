//! Relay lifecycle logic — stop, status.
//!
//! Returns results; does not print. Restart is the host process's
//! responsibility (systemd / Docker / ...).

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

        // Stale-lockfile paths are exercised by relay_lock's own unit tests
        // (which reach into the real ~/.mcpr/relay dir).
    }
}
