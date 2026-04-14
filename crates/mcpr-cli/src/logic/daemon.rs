//! Daemon status queries — gathers data for rendering, does not print.

use crate::proxy_lock;
use crate::render::{DaemonStatusError, DaemonStatusInfo};

/// Gather daemon status info for rendering.
///
/// Returns `Ok(DaemonStatusInfo)` when running, or an error variant
/// for stale/not-running states.  The caller handles rendering and
/// any cleanup (stale PID removal).
#[cfg(unix)]
pub fn get_status() -> Result<DaemonStatusInfo, DaemonStatusError> {
    use crate::daemon;

    match daemon::check_status() {
        daemon::DaemonStatus::Running(info) => {
            let uptime = chrono::Utc::now().timestamp() - info.started_at;
            let hours = uptime / 3600;
            let minutes = (uptime % 3600) / 60;
            let seconds = uptime % 60;

            let proxies = proxy_lock::list_proxies();
            let running_proxies: Vec<(String, proxy_lock::LockInfo)> = proxies
                .into_iter()
                .filter_map(|(name, status)| match status {
                    proxy_lock::LockStatus::Held(lock) => Some((name, lock)),
                    _ => None,
                })
                .collect();

            Ok(DaemonStatusInfo {
                pid: info.pid,
                hours,
                minutes,
                seconds,
                pid_file: daemon::pid_file_path(),
                log_file: daemon::daemon_log_path(),
                running_proxies,
            })
        }
        daemon::DaemonStatus::Stale(info) => {
            daemon::remove_pid_file();
            Err(DaemonStatusError::Stale { pid: info.pid })
        }
        daemon::DaemonStatus::NotRunning => Err(DaemonStatusError::NotRunning),
    }
}
