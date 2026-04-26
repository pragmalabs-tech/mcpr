//! Proxy lifecycle logic — start, stop, restart, reload.
//!
//! Returns results; does not print.

use std::path::Path;
use std::time::{Duration, Instant};

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
        .args(["proxy", "run", &config_path])
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

/// Outcome of a reload signal — set by the running proxy and read back
/// via `~/.mcpr/proxies/<name>/reload_result`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReloadOutcome {
    /// The proxy applied the new config.
    Applied,
    /// The proxy rejected the reload (e.g. an unsafe field changed).
    Rejected { message: String },
    /// SIGHUP was sent but no result file appeared in time.
    Timeout,
}

const RELOAD_TIMEOUT: Duration = Duration::from_secs(3);
const RELOAD_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Hot-reload a running proxy's config from `config_path`.
///
/// Writes the new snapshot, drops a fresh nonce in `reload_request`, sends
/// SIGHUP, and waits for the proxy to echo the nonce back in `reload_result`.
/// The proxy decides whether the change is live-applicable — if any field
/// outside the live-reloadable set changed, the proxy reports a rejection
/// and keeps running on the old config.
pub fn reload_proxy(name: &str, config_path: &Path) -> Result<ReloadOutcome, String> {
    let info = match proxy_lock::check_lock(name) {
        proxy_lock::LockStatus::Held(info) => info,
        proxy_lock::LockStatus::Stale(_) | proxy_lock::LockStatus::Free => {
            return Err(format!("proxy \"{name}\" is not running"));
        }
    };

    let contents = read_config_file(config_path)?;
    proxy_lock::snapshot_config(name, &contents)
        .map_err(|e| format!("failed to write config snapshot for \"{name}\": {e}"))?;

    let nonce = next_reload_nonce();
    proxy_lock::write_reload_request(name, nonce)
        .map_err(|e| format!("failed to write reload request for \"{name}\": {e}"))?;

    send_sighup(info.pid)?;

    Ok(wait_for_reload_result(
        name,
        nonce,
        RELOAD_TIMEOUT,
        RELOAD_POLL_INTERVAL,
    ))
}

fn next_reload_nonce() -> u64 {
    chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0) as u64
}

fn wait_for_reload_result(
    name: &str,
    nonce: u64,
    timeout: Duration,
    poll_interval: Duration,
) -> ReloadOutcome {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(result) = proxy_lock::read_reload_result(name)
            && result.nonce == nonce
        {
            return match result.status {
                proxy_lock::ReloadStatus::Applied => ReloadOutcome::Applied,
                proxy_lock::ReloadStatus::Rejected => ReloadOutcome::Rejected {
                    message: result.message,
                },
            };
        }
        if Instant::now() >= deadline {
            return ReloadOutcome::Timeout;
        }
        std::thread::sleep(poll_interval);
    }
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

    // ── reload_proxy ──────────────────────────────────────────

    #[test]
    fn reload_proxy__not_running_returns_err() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), "[mcp]\nurl=\"http://localhost:9000\"\n").unwrap();

        let err = reload_proxy("__nonexistent_reload_target_zzz__", tmp.path()).unwrap_err();

        assert!(
            err.contains("not running"),
            "expected 'not running' in err, got: {err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn reload_proxy__missing_config_file_returns_err() {
        // Lock points at the current process so `check_lock` returns Held,
        // which lets us exercise the config-read path. Path is bogus → err.
        let name = "__test_reload_proxy_missing_cfg__";
        proxy_lock::write_lock(name, 4242, "/tmp/x.toml", None).unwrap();

        let result = reload_proxy(name, std::path::Path::new("/nonexistent/missing.toml"));
        let _ = std::fs::remove_dir_all(proxy_lock_dir(name));

        let err = result.unwrap_err();
        assert!(err.contains("failed to read"), "got: {err}");
    }

    // ── wait_for_reload_result ────────────────────────────────

    #[test]
    fn wait_for_reload_result__applied_match_returns_immediately() {
        let name = "__test_wait_reload_applied__";
        proxy_lock::write_reload_result(name, 100, proxy_lock::ReloadStatus::Applied, "ok")
            .unwrap();

        let outcome = wait_for_reload_result(
            name,
            100,
            Duration::from_millis(500),
            Duration::from_millis(10),
        );
        let _ = std::fs::remove_dir_all(proxy_lock_dir(name));

        assert_eq!(outcome, ReloadOutcome::Applied);
    }

    #[test]
    fn wait_for_reload_result__rejected_carries_message() {
        let name = "__test_wait_reload_rejected__";
        proxy_lock::write_reload_result(
            name,
            200,
            proxy_lock::ReloadStatus::Rejected,
            "fields require restart: mcp",
        )
        .unwrap();

        let outcome = wait_for_reload_result(
            name,
            200,
            Duration::from_millis(500),
            Duration::from_millis(10),
        );
        let _ = std::fs::remove_dir_all(proxy_lock_dir(name));

        assert_eq!(
            outcome,
            ReloadOutcome::Rejected {
                message: "fields require restart: mcp".into(),
            }
        );
    }

    #[test]
    fn wait_for_reload_result__missing_file_times_out() {
        let name = "__test_wait_reload_missing__";
        let outcome = wait_for_reload_result(
            name,
            300,
            Duration::from_millis(150),
            Duration::from_millis(20),
        );
        assert_eq!(outcome, ReloadOutcome::Timeout);
    }

    #[test]
    fn wait_for_reload_result__stale_nonce_times_out() {
        // Existing result from a previous reload should not satisfy the
        // current request — only an exact nonce match counts.
        let name = "__test_wait_reload_stale__";
        proxy_lock::write_reload_result(name, 1, proxy_lock::ReloadStatus::Applied, "old").unwrap();

        let outcome = wait_for_reload_result(
            name,
            999,
            Duration::from_millis(150),
            Duration::from_millis(20),
        );
        let _ = std::fs::remove_dir_all(proxy_lock_dir(name));

        assert_eq!(outcome, ReloadOutcome::Timeout);
    }

    #[test]
    fn wait_for_reload_result__appears_during_poll() {
        let name = "__test_wait_reload_late__";
        let n = name.to_string();
        let writer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(60));
            proxy_lock::write_reload_result(&n, 555, proxy_lock::ReloadStatus::Applied, "ok")
                .unwrap();
        });

        let outcome = wait_for_reload_result(
            name,
            555,
            Duration::from_millis(500),
            Duration::from_millis(20),
        );
        writer.join().unwrap();
        let _ = std::fs::remove_dir_all(proxy_lock_dir(name));

        assert_eq!(outcome, ReloadOutcome::Applied);
    }

    fn proxy_lock_dir(name: &str) -> std::path::PathBuf {
        dirs::home_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .join(".mcpr")
            .join("proxies")
            .join(name)
    }

    #[cfg(unix)]
    #[test]
    fn delete_proxy__running_proxy_errors_without_removing() {
        let name = "__test_delete_logic_running__";
        proxy_lock::write_lock(name, 4242, "/tmp/x.toml", None).unwrap();
        assert!(proxy_lock::proxy_dir_exists(name));

        let err = delete_proxy(name).unwrap_err();
        assert!(err.contains("is running"));
        assert!(proxy_lock::proxy_dir_exists(name));

        let _ = proxy_lock::delete_proxy_dir(name);
    }
}
