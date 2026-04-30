//! Proxy lifecycle logic — stop, list, delete.
//!
//! Returns results; does not print. Restart / start are the host process's
//! responsibility (systemd `Restart=on-failure`, Docker `restart=always`,
//! k8s `restartPolicy`) — not mcpr's.

use crate::proxy_lock;

/// Result of stopping a proxy.
pub enum StopResult {
    /// Proxy was running and has been stopped.
    Stopped { name: String, pid: u32 },
    /// Lock was stale and has been cleaned up.
    StaleCleaned { name: String },
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

/// List all proxies and their status.
pub fn list_proxies() -> Vec<(String, proxy_lock::LockStatus)> {
    proxy_lock::list_proxies()
}

/// Delete a single stopped proxy by name. Errors if the proxy is running —
/// callers must stop it explicitly first.
pub fn delete_proxy(name: &str) -> Result<(), String> {
    match proxy_lock::check_lock(name) {
        proxy_lock::LockStatus::Held(_) => {
            return Err(format!(
                "proxy \"{name}\" is running. Stop it first with `mcpr proxy stop {name}`."
            ));
        }
        proxy_lock::LockStatus::Stale(_) => {
            proxy_lock::remove_lock(name);
        }
        proxy_lock::LockStatus::Free => {
            if !proxy_lock::proxy_dir_exists(name) {
                return Err(format!("proxy \"{name}\" not found."));
            }
        }
    }

    proxy_lock::delete_proxy_dir(name)
        .map_err(|e| format!("failed to remove proxy \"{name}\" directory: {e}"))
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

        delete_proxy(name).unwrap();
        assert!(!proxy_lock::proxy_dir_exists(name));
    }

    #[cfg(unix)]
    #[test]
    fn delete_proxy__running_proxy_errors_without_removing() {
        let name = "__test_delete_logic_running__";
        proxy_lock::write_lock(name, 4242, "/tmp/x.toml").unwrap();
        assert!(proxy_lock::proxy_dir_exists(name));

        let err = delete_proxy(name).unwrap_err();
        assert!(err.contains("is running"));
        assert!(proxy_lock::proxy_dir_exists(name));

        let _ = proxy_lock::delete_proxy_dir(name);
    }
}
