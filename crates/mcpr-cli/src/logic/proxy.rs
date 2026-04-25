//! Proxy lifecycle logic — start, stop, restart, reload.
//!
//! Returns results; does not print.

use std::path::Path;

use crate::proxy_lock;

/// Result of stopping a proxy.
pub enum StopResult {
    /// Proxy was running and has been stopped.
    Stopped { name: String, pid: u32 },
    /// Lock was stale and has been cleaned up.
    StaleCleaned { name: String },
}

/// Start a proxy from its saved config snapshot.
///
/// Caller must ensure no proxy with the same name is currently running —
/// `mcpr proxy run` errors out on conflict. This function is the internal
/// re-spawn used by `start` and `restart`, both of which stop any existing
/// proxy before calling here.
pub fn start_proxy(name: &str) -> Result<(), String> {
    // Verify config snapshot exists before attempting re-launch.
    proxy_lock::read_snapshot(name)
        .map_err(|e| format!("no config snapshot for proxy \"{name}\": {e}"))?;

    let config_path = proxy_lock::config_snapshot_path(name).display().to_string();

    // Re-launch by running the mcpr binary with proxy run.
    let exe = std::env::current_exe().map_err(|e| format!("cannot find mcpr binary: {e}"))?;

    let status = std::process::Command::new(exe)
        .args(["proxy", "run", "--config", &config_path])
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

/// Restart a single proxy. Process kill + respawn.
///
/// If `config_path` is provided, the snapshot is refreshed from it before the
/// respawn; otherwise the existing snapshot is reused.
pub fn restart_proxy(name: &str, config_path: Option<&Path>) -> Result<(), String> {
    // Validate and read the new config before touching the running process —
    // a bad path should not leave the proxy down.
    let new_snapshot = match config_path {
        Some(path) => Some(read_config_file(path)?),
        None => None,
    };

    proxy_lock::stop_proxy(name);

    if let Some(contents) = new_snapshot {
        proxy_lock::snapshot_config(name, &contents)
            .map_err(|e| format!("failed to write config snapshot for \"{name}\": {e}"))?;
    }

    start_proxy(name)
}

/// Restart all running proxies. Returns count of restarted proxies.
pub fn restart_all_proxies() -> Result<usize, String> {
    let proxies = proxy_lock::list_proxies();
    let mut restarted = 0;
    for (name, status) in &proxies {
        match status {
            proxy_lock::LockStatus::Held(_) | proxy_lock::LockStatus::Stale(_) => {
                restart_proxy(name, None)?;
                restarted += 1;
            }
            proxy_lock::LockStatus::Free => {}
        }
    }
    Ok(restarted)
}

/// Hot-reload a running proxy's config from `config_path`.
///
/// Refreshes the on-disk snapshot from the given file, then sends SIGHUP to
/// the proxy process. The proxy itself decides whether the change is
/// live-applicable — if any field outside the live-reloadable set changed,
/// the proxy logs the rejection and keeps running with the old config.
pub fn reload_proxy(name: &str, config_path: &Path) -> Result<(), String> {
    let info = match proxy_lock::check_lock(name) {
        proxy_lock::LockStatus::Held(info) => info,
        proxy_lock::LockStatus::Stale(_) | proxy_lock::LockStatus::Free => {
            return Err(format!("proxy \"{name}\" is not running"));
        }
    };

    let contents = read_config_file(config_path)?;
    proxy_lock::snapshot_config(name, &contents)
        .map_err(|e| format!("failed to write config snapshot for \"{name}\": {e}"))?;

    send_sighup(info.pid)
}

fn read_config_file(path: &Path) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|e| format!("failed to read {}: {e}", path.display()))
}

#[cfg(unix)]
fn send_sighup(pid: u32) -> Result<(), String> {
    use nix::sys::signal::{Signal, kill};
    use nix::unistd::Pid;
    kill(Pid::from_raw(pid as i32), Signal::SIGHUP)
        .map_err(|e| format!("failed to send SIGHUP to pid {pid}: {e}"))
}

#[cfg(not(unix))]
fn send_sighup(_pid: u32) -> Result<(), String> {
    Err("reload is not supported on this platform".to_string())
}

/// List all proxies and their status.
pub fn list_proxies() -> Vec<(String, proxy_lock::LockStatus)> {
    proxy_lock::list_proxies()
}

/// Result of deleting a proxy.
#[derive(Debug)]
pub struct DeleteResult {
    pub name: String,
    /// True if the proxy was running and had to be stopped before removal.
    pub was_running: bool,
}

/// Delete a single proxy by name.  Stops it first if it is running.
pub fn delete_proxy(name: &str) -> Result<DeleteResult, String> {
    let was_running = match proxy_lock::check_lock(name) {
        proxy_lock::LockStatus::Held(_) => {
            proxy_lock::stop_proxy(name);
            true
        }
        proxy_lock::LockStatus::Stale(_) => {
            proxy_lock::remove_lock(name);
            false
        }
        proxy_lock::LockStatus::Free => {
            if !proxy_lock::proxy_dir_exists(name) {
                return Err(format!("proxy \"{name}\" not found."));
            }
            false
        }
    };

    proxy_lock::delete_proxy_dir(name)
        .map_err(|e| format!("failed to remove proxy \"{name}\" directory: {e}"))?;

    Ok(DeleteResult {
        name: name.to_string(),
        was_running,
    })
}

/// Delete every known proxy.  Stops any that are running first.
pub fn delete_all_proxies() -> Result<Vec<DeleteResult>, String> {
    let proxies = proxy_lock::list_proxies();
    let mut deleted = Vec::with_capacity(proxies.len());
    for (name, _) in proxies {
        deleted.push(delete_proxy(&name)?);
    }
    Ok(deleted)
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;

    #[test]
    fn delete_proxy__missing_returns_not_found() {
        let err = delete_proxy("__test_delete_logic_missing_zzz__").unwrap_err();
        assert!(err.contains("not found"));
    }

    #[test]
    fn delete_proxy__stopped_proxy_removes_dir() {
        let name = "__test_delete_logic_stopped__";
        proxy_lock::snapshot_config(name, "[mcp]\nurl=\"x\"\n").unwrap();
        assert!(proxy_lock::proxy_dir_exists(name));

        let result = delete_proxy(name).unwrap();
        assert_eq!(result.name, name);
        assert!(!result.was_running);
        assert!(!proxy_lock::proxy_dir_exists(name));
    }
}
