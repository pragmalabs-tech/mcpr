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

/// Path to the upstream MCP URL file for a proxy.
fn upstream_url_path(name: &str) -> PathBuf {
    proxy_dir(name).join("upstream_url")
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

/// Write the upstream MCP URL for a proxy (called at startup).
pub fn write_upstream_url(name: &str, url: &str) -> std::io::Result<()> {
    let dir = proxy_dir(name);
    fs::create_dir_all(&dir)?;
    fs::write(upstream_url_path(name), url)
}

/// Read the upstream MCP URL for a proxy, if one was written.
pub fn read_upstream_url(name: &str) -> Option<String> {
    fs::read_to_string(upstream_url_path(name))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Read lock info for a proxy by name.
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
pub fn write_lock(name: &str, port: u16, config_path: &str) -> std::io::Result<()> {
    let dir = proxy_dir(name);
    fs::create_dir_all(&dir)?;

    let pid = std::process::id();
    let started_at = chrono::Utc::now().timestamp();
    let content = format!("{pid}\n{port}\n{started_at}\n{config_path}\n");

    fs::write(lock_path(name), content)
}

/// Remove the lockfile for a proxy.
pub fn remove_lock(name: &str) {
    let _ = fs::remove_file(lock_path(name));
}

/// Remove the entire on-disk state directory for a proxy
/// (`~/.mcpr/proxies/<name>/` — lock, snapshot, logs, tunnel/upstream URLs).
///
/// Caller must ensure the proxy process has been stopped first.
pub fn delete_proxy_dir(name: &str) -> std::io::Result<()> {
    let dir = proxy_dir(name);
    match fs::remove_dir_all(&dir) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Whether the on-disk directory for a proxy exists.
pub fn proxy_dir_exists(name: &str) -> bool {
    proxy_dir(name).is_dir()
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

// ── Reload IPC ───────────────────────────────────────────────────────
//
// `mcpr proxy reload` is a 3-step handshake:
//   1. CLI writes the new config snapshot and a `reload_request` containing
//      a fresh nonce, then sends SIGHUP.
//   2. The running proxy reads the nonce, applies (or rejects) the snapshot,
//      and atomically writes a `reload_result` echoing the same nonce.
//   3. CLI polls `reload_result` for a matching nonce, then prints success
//      or the rejection reason. Without the nonce the CLI could mistake a
//      stale result file for confirmation of the current request.

/// Outcome of a reload signal — written by the proxy, read by the CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReloadStatus {
    Applied,
    Rejected,
}

impl ReloadStatus {
    fn as_str(&self) -> &'static str {
        match self {
            ReloadStatus::Applied => "applied",
            ReloadStatus::Rejected => "rejected",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReloadResult {
    pub nonce: u64,
    pub status: ReloadStatus,
    pub message: String,
}

fn reload_request_path(name: &str) -> PathBuf {
    proxy_dir(name).join("reload_request")
}

fn reload_result_path(name: &str) -> PathBuf {
    proxy_dir(name).join("reload_result")
}

/// CLI-side: announce a pending reload by writing the request nonce.
pub fn write_reload_request(name: &str, nonce: u64) -> std::io::Result<()> {
    let dir = proxy_dir(name);
    fs::create_dir_all(&dir)?;
    fs::write(reload_request_path(name), nonce.to_string())
}

/// Proxy-side: read the most recent reload request nonce. Returns an error
/// when SIGHUP arrived without a matching request file (e.g. external `kill
/// -HUP` rather than `mcpr proxy reload`).
pub fn read_reload_request(name: &str) -> std::io::Result<u64> {
    let s = fs::read_to_string(reload_request_path(name))?;
    s.trim()
        .parse::<u64>()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

/// Proxy-side: write the outcome of a reload atomically (write-then-rename),
/// echoing the nonce so the CLI can ignore stale results from prior reloads.
pub fn write_reload_result(
    name: &str,
    nonce: u64,
    status: ReloadStatus,
    message: &str,
) -> std::io::Result<()> {
    let dir = proxy_dir(name);
    fs::create_dir_all(&dir)?;
    // Newlines would corrupt the simple line-based format below.
    let safe_msg = message.replace('\n', " ");
    let body = format!("{}\n{}\n{}\n", nonce, status.as_str(), safe_msg);
    let final_path = reload_result_path(name);
    let tmp_path = dir.join("reload_result.tmp");
    fs::write(&tmp_path, body)?;
    fs::rename(tmp_path, final_path)
}

/// CLI-side: read the latest reload outcome. Returns `None` when the file
/// is missing or malformed; callers should keep polling in either case.
pub fn read_reload_result(name: &str) -> Option<ReloadResult> {
    let s = fs::read_to_string(reload_result_path(name)).ok()?;
    let (nonce_line, rest) = s.split_once('\n')?;
    let (status_line, rest) = rest.split_once('\n')?;
    let nonce: u64 = nonce_line.trim().parse().ok()?;
    let status = match status_line.trim() {
        "applied" => ReloadStatus::Applied,
        "rejected" => ReloadStatus::Rejected,
        _ => return None,
    };
    let message = rest.trim_end_matches('\n').to_string();
    Some(ReloadResult {
        nonce,
        status,
        message,
    })
}

/// List all proxies that have a lock directory, with their lock status.
///
/// Stale lockfiles (process dead) are removed lazily as a side effect.
/// `list_proxies` is the only consumer that needs current state, so it
/// owns the GC. The returned vec still includes stale entries so callers
/// (`mcpr proxy list`) can show "just cleaned up X".
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
        if matches!(status, LockStatus::Stale(_)) {
            remove_lock(&name);
        }
        result.push((name, status));
    }

    result.sort_by(|a, b| a.0.cmp(&b.0));
    result
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

// ── Process helpers ────────────────────────────────────────────────

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

    Some(LockInfo {
        pid,
        port,
        started_at,
        config_path,
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

    // ── delete_proxy_dir ─────────────────────────────────────

    #[test]
    fn delete_proxy_dir__removes_existing_dir() {
        let name = "__test_delete_existing__";
        write_tunnel_url(name, "http://localhost:9999").unwrap();
        assert!(proxy_dir_exists(name));

        delete_proxy_dir(name).unwrap();
        assert!(!proxy_dir_exists(name));
    }

    #[test]
    fn delete_proxy_dir__missing_is_ok() {
        let name = "__test_delete_missing_xyz__";
        assert!(!proxy_dir_exists(name));
        delete_proxy_dir(name).unwrap();
    }

    #[test]
    fn delete_proxy_dir__removes_all_files() {
        let name = "__test_delete_full__";
        let dir = proxy_dir(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("lock"), "1\n2\n3\n/p\n").unwrap();
        fs::write(dir.join("config.toml"), "x=1").unwrap();
        fs::write(dir.join("proxy.log"), "log").unwrap();

        delete_proxy_dir(name).unwrap();
        assert!(!proxy_dir_exists(name));
    }

    // ── reload IPC ───────────────────────────────────────────

    #[test]
    fn reload_request__roundtrip() {
        let name = "__test_reload_request_roundtrip__";
        write_reload_request(name, 12345).unwrap();
        let got = read_reload_request(name).unwrap();
        let _ = fs::remove_dir_all(proxy_dir(name));
        assert_eq!(got, 12345);
    }

    #[test]
    fn reload_request__missing_returns_err() {
        let err = read_reload_request("__nonexistent_reload_req_xyz__");
        assert!(err.is_err());
    }

    #[test]
    fn reload_request__malformed_returns_err() {
        let name = "__test_reload_request_malformed__";
        let dir = proxy_dir(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("reload_request"), "not-a-number").unwrap();
        let result = read_reload_request(name);
        let _ = fs::remove_dir_all(&dir);
        assert!(result.is_err());
    }

    #[test]
    fn reload_result__roundtrip_applied() {
        let name = "__test_reload_result_applied__";
        write_reload_result(name, 999, ReloadStatus::Applied, "ok").unwrap();
        let got = read_reload_result(name).unwrap();
        let _ = fs::remove_dir_all(proxy_dir(name));
        assert_eq!(
            got,
            ReloadResult {
                nonce: 999,
                status: ReloadStatus::Applied,
                message: "ok".to_string(),
            }
        );
    }

    #[test]
    fn reload_result__roundtrip_rejected_with_message() {
        let name = "__test_reload_result_rejected__";
        let reason = "fields require restart: mcp, port";
        write_reload_result(name, 42, ReloadStatus::Rejected, reason).unwrap();
        let got = read_reload_result(name).unwrap();
        let _ = fs::remove_dir_all(proxy_dir(name));
        assert_eq!(got.nonce, 42);
        assert_eq!(got.status, ReloadStatus::Rejected);
        assert_eq!(got.message, reason);
    }

    #[test]
    fn reload_result__newlines_in_message_are_squashed() {
        let name = "__test_reload_result_newlines__";
        write_reload_result(name, 7, ReloadStatus::Rejected, "line1\nline2").unwrap();
        let got = read_reload_result(name).unwrap();
        let _ = fs::remove_dir_all(proxy_dir(name));
        assert_eq!(got.message, "line1 line2");
    }

    #[test]
    fn reload_result__missing_returns_none() {
        assert!(read_reload_result("__nonexistent_reload_result_xyz__").is_none());
    }

    #[test]
    fn reload_result__malformed_returns_none() {
        let name = "__test_reload_result_malformed__";
        let dir = proxy_dir(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("reload_result"), "12345\nbogus\nmsg\n").unwrap();
        let result = read_reload_result(name);
        let _ = fs::remove_dir_all(&dir);
        assert!(result.is_none());
    }

    #[test]
    fn reload_result__truncated_returns_none() {
        let name = "__test_reload_result_truncated__";
        let dir = proxy_dir(name);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("reload_result"), "12345\n").unwrap();
        let result = read_reload_result(name);
        let _ = fs::remove_dir_all(&dir);
        assert!(result.is_none());
    }

    #[test]
    fn reload_result__overwrite_keeps_latest() {
        let name = "__test_reload_result_overwrite__";
        write_reload_result(name, 1, ReloadStatus::Applied, "first").unwrap();
        write_reload_result(name, 2, ReloadStatus::Rejected, "second").unwrap();
        let got = read_reload_result(name).unwrap();
        let _ = fs::remove_dir_all(proxy_dir(name));
        assert_eq!(got.nonce, 2);
        assert_eq!(got.status, ReloadStatus::Rejected);
        assert_eq!(got.message, "second");
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
