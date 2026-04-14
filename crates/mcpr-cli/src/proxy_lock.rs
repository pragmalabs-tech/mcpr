//! Per-proxy lockfile management for multi-proxy mode.
//!
//! Each proxy launched via `mcpr proxy run` gets its own directory under
//! `~/.mcpr/proxies/{name}/` containing:
//! - `lock` — PID, port, timestamp, config path (used for lifecycle management)
//! - `config.toml` — snapshot of the config at launch time (used for restart)
//! - `proxy.log` — stdout/stderr redirect for the background process

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Information stored in a proxy lockfile.
#[derive(Debug, Clone)]
pub struct LockInfo {
    pub pid: u32,
    pub port: u16,
    pub started_at: i64,
    pub config_path: String,
    /// PID of the daemon that was running when this proxy started.
    /// `None` for lockfiles written before this field was added.
    pub daemon_pid: Option<u32>,
}

/// Current state of a proxy lock.
pub enum LockStatus {
    /// No lockfile exists.
    Free,
    /// Lockfile exists and the process is alive.
    Held(LockInfo),
    /// Lockfile exists but the process is dead.
    Stale(LockInfo),
}

// ── Path helpers ──────────────────────────────────────────────────────

pub(crate) fn mcpr_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".mcpr")
}

fn proxies_dir() -> PathBuf {
    mcpr_dir().join("proxies")
}

fn proxy_dir(name: &str) -> PathBuf {
    proxies_dir().join(name)
}

fn lock_path(name: &str) -> PathBuf {
    proxy_dir(name).join("lock")
}

/// Path to the config snapshot for a proxy.
pub fn config_snapshot_path(name: &str) -> PathBuf {
    proxy_dir(name).join("config.toml")
}

/// Path to the log file for a proxy.
pub fn log_path(name: &str) -> PathBuf {
    proxy_dir(name).join("proxy.log")
}

/// Path to the tunnel URL file for a proxy.
fn tunnel_url_path(name: &str) -> PathBuf {
    proxy_dir(name).join("tunnel_url")
}

/// Write the tunnel/public URL for a proxy (called after tunnel resolution).
pub fn write_tunnel_url(name: &str, url: &str) -> std::io::Result<()> {
    let dir = proxy_dir(name);
    fs::create_dir_all(&dir)?;
    fs::write(tunnel_url_path(name), url)
}

/// Read the tunnel URL for a proxy, if one was written.
pub fn read_tunnel_url(name: &str) -> Option<String> {
    fs::read_to_string(tunnel_url_path(name))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Read lock info for a proxy by name (used by daemon readiness check).
pub fn read_lock_info(name: &str) -> Option<LockInfo> {
    read_lock_file(&lock_path(name))
}

// ── Lock operations ──────────────────────────────────────────────────

/// Check the lock status for a named proxy.
pub fn check_lock(name: &str) -> LockStatus {
    let path = lock_path(name);
    let info = match read_lock_file(&path) {
        Some(info) => info,
        None => return LockStatus::Free,
    };

    if is_process_alive(info.pid) {
        LockStatus::Held(info)
    } else {
        LockStatus::Stale(info)
    }
}

/// Write a lockfile for a proxy after successful port bind.
pub fn write_lock(
    name: &str,
    port: u16,
    config_path: &str,
    daemon_pid: Option<u32>,
) -> std::io::Result<()> {
    let dir = proxy_dir(name);
    fs::create_dir_all(&dir)?;

    let pid = std::process::id();
    let started_at = chrono::Utc::now().timestamp();
    let dpid = daemon_pid.map(|p| p.to_string()).unwrap_or_default();
    let content = format!("{pid}\n{port}\n{started_at}\n{config_path}\n{dpid}\n");

    fs::write(lock_path(name), content)
}

/// Remove the lockfile for a proxy.
pub fn remove_lock(name: &str) {
    let _ = fs::remove_file(lock_path(name));
}

/// Save a config snapshot for a proxy (used for restart).
pub fn snapshot_config(name: &str, content: &str) -> std::io::Result<()> {
    let dir = proxy_dir(name);
    fs::create_dir_all(&dir)?;
    fs::write(config_snapshot_path(name), content)
}

/// Read the saved config snapshot for a proxy.
pub fn read_snapshot(name: &str) -> std::io::Result<String> {
    fs::read_to_string(config_snapshot_path(name))
}

/// List all proxies that have a lock directory, with their lock status.
pub fn list_proxies() -> Vec<(String, LockStatus)> {
    let dir = proxies_dir();
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(_) => return Vec::new(),
    };

    let mut result = Vec::new();
    for entry in entries.flatten() {
        if !entry.path().is_dir() {
            continue;
        }
        let name = match entry.file_name().into_string() {
            Ok(n) => n,
            Err(_) => continue,
        };
        let status = check_lock(&name);
        result.push((name, status));
    }

    result.sort_by(|a, b| a.0.cmp(&b.0));
    result
}

/// Check if the daemon that this proxy was started under is still alive.
/// Returns `true` if the daemon PID is present and the process is running,
/// or if no daemon PID is recorded (backward compat).
pub fn check_daemon_alive(daemon_pid: Option<u32>) -> bool {
    match daemon_pid {
        Some(pid) => is_process_alive(pid),
        None => true, // No daemon PID recorded — assume okay.
    }
}

/// Stop a running proxy by name: send SIGTERM and wait for exit.
/// Returns true if the process was stopped, false if it wasn't running.
pub fn stop_proxy(name: &str) -> bool {
    match check_lock(name) {
        LockStatus::Held(info) => {
            send_sigterm(info.pid);
            wait_for_exit(info.pid, Duration::from_secs(10));
            remove_lock(name);
            true
        }
        LockStatus::Stale(_) => {
            remove_lock(name);
            false
        }
        LockStatus::Free => false,
    }
}

/// Stop all running proxies. Returns names of proxies that were stopped.
pub fn stop_all_proxies() -> Vec<String> {
    let mut stopped = Vec::new();
    for (name, status) in list_proxies() {
        match status {
            LockStatus::Held(info) => {
                send_sigterm(info.pid);
                wait_for_exit(info.pid, Duration::from_secs(10));
                remove_lock(&name);
                stopped.push(name);
            }
            LockStatus::Stale(_) => {
                remove_lock(&name);
            }
            LockStatus::Free => {}
        }
    }
    stopped
}

/// Mark all running proxies as stopped (remove lockfiles without killing).
/// Used by `mcpr restart` to clean up proxy locks when restarting the daemon.
pub fn mark_all_stopped() -> Vec<String> {
    let mut marked = Vec::new();
    for (name, status) in list_proxies() {
        match status {
            LockStatus::Held(info) => {
                send_sigterm(info.pid);
                wait_for_exit(info.pid, Duration::from_secs(10));
                remove_lock(&name);
                marked.push(name);
            }
            LockStatus::Stale(_) => {
                remove_lock(&name);
            }
            LockStatus::Free => {}
        }
    }
    marked
}

// ── Stdio redirect ──────────────────────────────────────────────────

/// Redirect stdin/stdout/stderr for a background proxy process.
#[cfg(unix)]
pub fn redirect_stdio(name: &str) -> std::io::Result<()> {
    use nix::unistd::dup2;
    use std::os::unix::io::AsRawFd;

    let log = log_path(name);
    if let Some(parent) = log.parent() {
        fs::create_dir_all(parent)?;
    }

    let dev_null = fs::OpenOptions::new().read(true).open("/dev/null")?;
    let log_file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)?;

    dup2(dev_null.as_raw_fd(), 0).map_err(|e| std::io::Error::other(format!("dup2 stdin: {e}")))?;
    dup2(log_file.as_raw_fd(), 1)
        .map_err(|e| std::io::Error::other(format!("dup2 stdout: {e}")))?;
    dup2(log_file.as_raw_fd(), 2)
        .map_err(|e| std::io::Error::other(format!("dup2 stderr: {e}")))?;

    Ok(())
}

// ── Process helpers (shared with daemon.rs) ──────────────────────────

/// Check if a process with the given PID is alive.
#[cfg(unix)]
pub(crate) fn is_process_alive(pid: u32) -> bool {
    use nix::sys::signal;
    use nix::unistd::Pid;
    signal::kill(Pid::from_raw(pid as i32), None).is_ok()
}

#[cfg(not(unix))]
pub(crate) fn is_process_alive(_pid: u32) -> bool {
    false
}

/// Send SIGTERM to a process.
#[cfg(unix)]
pub(crate) fn send_sigterm(pid: u32) {
    use nix::sys::signal::{Signal, kill};
    use nix::unistd::Pid;
    let _ = kill(Pid::from_raw(pid as i32), Signal::SIGTERM);
}

#[cfg(not(unix))]
pub(crate) fn send_sigterm(_pid: u32) {}

/// Wait for a process to exit, polling every 100ms.
pub(crate) fn wait_for_exit(pid: u32, timeout: Duration) {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if !is_process_alive(pid) {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    eprintln!(
        "warning: proxy (PID: {pid}) did not exit within {}s",
        timeout.as_secs()
    );
}

/// Parse a lockfile.
fn read_lock_file(path: &Path) -> Option<LockInfo> {
    let content = fs::read_to_string(path).ok()?;
    let mut lines = content.lines();

    let pid: u32 = lines.next()?.parse().ok()?;
    let port: u16 = lines.next()?.parse().ok()?;
    let started_at: i64 = lines.next()?.parse().ok()?;
    let config_path: String = lines.next()?.to_string();
    // Optional 5th line: daemon PID (backward compat with old lockfiles).
    let daemon_pid: Option<u32> = lines.next().and_then(|s| s.parse().ok());

    Some(LockInfo {
        pid,
        port,
        started_at,
        config_path,
        daemon_pid,
    })
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;

    #[test]
    fn read_lock_file__roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("test.lock");

        let pid = std::process::id();
        let ts = chrono::Utc::now().timestamp();
        let content = format!("{pid}\n8080\n{ts}\n/tmp/test.toml\n");
        fs::write(&lock, &content).unwrap();

        let info = read_lock_file(&lock).unwrap();
        assert_eq!(info.pid, pid);
        assert_eq!(info.port, 8080);
        assert_eq!(info.started_at, ts);
        assert_eq!(info.config_path, "/tmp/test.toml");
    }

    #[test]
    fn read_lock_file__missing_returns_none() {
        let info = read_lock_file(Path::new("/nonexistent/path/lock"));
        assert!(info.is_none());
    }

    #[test]
    fn read_lock_file__malformed_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("bad.lock");
        fs::write(&lock, "12345\n8080\n").unwrap();
        assert!(read_lock_file(&lock).is_none());
    }

    #[test]
    fn read_lock_file__write_and_read() {
        let dir = tempfile::tempdir().unwrap();
        let proxy_dir = dir.path().join("test-proxy");
        fs::create_dir_all(&proxy_dir).unwrap();
        let lock_file = proxy_dir.join("lock");

        let pid = std::process::id();
        let started_at = chrono::Utc::now().timestamp();
        let content = format!("{pid}\n3001\n{started_at}\n/tmp/search.toml\n");
        fs::write(&lock_file, &content).unwrap();

        let info = read_lock_file(&lock_file).unwrap();
        assert_eq!(info.pid, pid);
        assert_eq!(info.port, 3001);
        assert_eq!(info.config_path, "/tmp/search.toml");
    }

    #[test]
    fn snapshot_config__roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let snapshot_path = dir.path().join("config.toml");
        let content = "[mcp]\nurl = \"http://localhost:9000\"\n";
        fs::write(&snapshot_path, content).unwrap();

        let read_back = fs::read_to_string(&snapshot_path).unwrap();
        assert_eq!(read_back, content);
    }

    #[test]
    fn check_lock__free_when_no_dir() {
        let status = check_lock("nonexistent-test-proxy-abc123");
        assert!(matches!(status, LockStatus::Free));
    }

    #[cfg(unix)]
    #[test]
    fn check_lock__stale_when_pid_dead() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("lock");

        let ts = chrono::Utc::now().timestamp();
        let content = format!("99999999\n3001\n{ts}\n/tmp/test.toml\n");
        fs::write(&lock, &content).unwrap();

        let info = read_lock_file(&lock).unwrap();
        assert_eq!(info.pid, 99999999);
        assert!(!is_process_alive(99999999));
    }

    #[cfg(unix)]
    #[test]
    fn check_lock__held_when_pid_alive() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("lock");

        let pid = std::process::id();
        let ts = chrono::Utc::now().timestamp();
        let content = format!("{pid}\n3001\n{ts}\n/tmp/test.toml\n");
        fs::write(&lock, &content).unwrap();

        let info = read_lock_file(&lock).unwrap();
        assert!(is_process_alive(info.pid));
    }

    // ── tunnel URL ────────────────────────────────────────────

    #[test]
    fn tunnel_url__roundtrip() {
        let name = "__test_tunnel_roundtrip__";
        let url = "https://myapp.tunnel.mcpr.app";
        write_tunnel_url(name, url).unwrap();
        let read_back = read_tunnel_url(name);
        // clean up
        let _ = fs::remove_dir_all(proxy_dir(name));
        assert_eq!(read_back, Some(url.to_string()));
    }

    #[test]
    fn tunnel_url__localhost_roundtrip() {
        let name = "__test_tunnel_localhost__";
        let url = "http://localhost:3000";
        write_tunnel_url(name, url).unwrap();
        let read_back = read_tunnel_url(name);
        let _ = fs::remove_dir_all(proxy_dir(name));
        assert_eq!(read_back, Some(url.to_string()));
    }

    #[test]
    fn tunnel_url__missing_returns_none() {
        assert!(read_tunnel_url("__nonexistent_proxy_xyz__").is_none());
    }

    #[test]
    fn tunnel_url__empty_file_returns_none() {
        let name = "__test_tunnel_empty__";
        let dir = proxy_dir(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("tunnel_url"), "").unwrap();
        let result = read_tunnel_url(name);
        let _ = fs::remove_dir_all(dir);
        assert!(result.is_none());
    }

    #[test]
    fn tunnel_url__trims_whitespace() {
        let name = "__test_tunnel_trim__";
        let dir = proxy_dir(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("tunnel_url"), "  https://x.tunnel.mcpr.app\n").unwrap();
        let result = read_tunnel_url(name);
        let _ = fs::remove_dir_all(dir);
        assert_eq!(result, Some("https://x.tunnel.mcpr.app".to_string()));
    }
}
