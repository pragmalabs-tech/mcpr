//! Proxy lifecycle logic — start, stop, restart.
//!
//! Returns results; does not print.

use crate::proxy_lock;

/// Result of stopping a proxy.
pub enum StopResult {
    /// Proxy was running and has been stopped.
    Stopped { name: String, pid: u32 },
    /// Lock was stale and has been cleaned up.
    StaleCleaned { name: String },
}

/// Start a proxy from its saved config snapshot.
pub fn start_proxy(name: &str) -> Result<(), String> {
    // Verify config snapshot exists before attempting re-launch.
    proxy_lock::read_snapshot(name)
        .map_err(|e| format!("no config snapshot for proxy \"{name}\": {e}"))?;

    let config_path = proxy_lock::config_snapshot_path(name).display().to_string();

    // Re-launch by running the mcpr binary with proxy run.
    let exe = std::env::current_exe().map_err(|e| format!("cannot find mcpr binary: {e}"))?;

    let status = std::process::Command::new(exe)
        .args(["proxy", "run", &config_path, "--replace"])
        .status()
        .map_err(|e| format!("failed to spawn proxy \"{name}\": {e}"))?;

    if !status.success() {
        return Err(format!("proxy \"{name}\" failed to start"));
    }

    Ok(())
}

/// Stop a single proxy by name.
pub fn stop_proxy(name: &str) -> Result<StopResult, String> {
    match proxy_lock::check_lock(name) {
        proxy_lock::LockStatus::Held(info) => {
            let pid = info.pid;
            proxy_lock::stop_proxy(name);
            Ok(StopResult::Stopped {
                name: name.to_string(),
                pid,
            })
        }
        proxy_lock::LockStatus::Stale(_) => {
            proxy_lock::remove_lock(name);
            Ok(StopResult::StaleCleaned {
                name: name.to_string(),
            })
        }
        proxy_lock::LockStatus::Free => Err(format!("proxy \"{}\" is not running.", name)),
    }
}

/// Stop all running proxies.  Returns list of stopped names.
pub fn stop_all_proxies() -> Vec<String> {
    proxy_lock::stop_all_proxies()
}

/// Restart a single proxy from its saved config snapshot.
pub fn restart_proxy(name: &str) -> Result<(), String> {
    proxy_lock::stop_proxy(name);
    start_proxy(name)
}

/// Restart all running proxies. Returns count of restarted proxies.
pub fn restart_all_proxies() -> Result<usize, String> {
    let proxies = proxy_lock::list_proxies();
    let mut restarted = 0;
    for (name, status) in &proxies {
        match status {
            proxy_lock::LockStatus::Held(_) | proxy_lock::LockStatus::Stale(_) => {
                restart_proxy(name)?;
                restarted += 1;
            }
            proxy_lock::LockStatus::Free => {}
        }
    }
    Ok(restarted)
}

/// List all proxies and their status.
pub fn list_proxies() -> Vec<(String, proxy_lock::LockStatus)> {
    proxy_lock::list_proxies()
}
