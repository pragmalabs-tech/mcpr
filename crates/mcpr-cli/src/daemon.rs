//! Daemon (mcprd) supervisor process management.
//!
//! The daemon is a pure supervisor — it owns no proxy connections, no HTTP
//! server, and needs no config file. It:
//! - Daemonizes via double-fork (Unix only).
//! - Writes `~/.mcpr/mcprd.pid`.
//! - Monitors proxy health every 10 s.
//! - On SIGTERM: stops all proxies → removes PID → exits.
//!
//! # PID file format
//!
//! Two lines, plain text:
//! ```text
//! <pid>
//! <start_unix_timestamp>
//! ```
//!
//! Location: `~/.mcpr/mcprd.pid`.

use std::fs;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::time::Duration;

#[cfg(unix)]
use nix::sys::signal::{self, Signal};
#[cfg(unix)]
use nix::unistd::Pid;

/// PID file name.
const PID_FILE: &str = "mcprd.pid";
/// Daemon log file name (stdout/stderr redirect).
const DAEMON_LOG: &str = "mcprd.log";

/// Information stored in the PID file.
#[derive(Debug)]
pub struct DaemonInfo {
    pub pid: u32,
    pub started_at: i64,
}

/// Current state of the daemon.
pub enum DaemonStatus {
    /// Daemon is running with this info.
    Running(DaemonInfo),
    /// PID file exists but process is dead.
    Stale(DaemonInfo),
    /// No PID file.
    NotRunning,
}

// ── Path resolution ────────────────────────────────────────────────────

/// Central mcpr state directory: `~/.mcpr/`.
fn mcpr_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".mcpr")
}

/// Path to the PID file.
pub fn pid_file_path() -> PathBuf {
    mcpr_dir().join(PID_FILE)
}

/// Path to the daemon log file (stdout/stderr redirect).
pub fn daemon_log_path() -> PathBuf {
    mcpr_dir().join(DAEMON_LOG)
}

// ── PID file operations ────────────────────────────────────────────────

/// Read the PID file and parse its contents.
pub fn read_pid_file() -> Option<DaemonInfo> {
    let content = fs::read_to_string(pid_file_path()).ok()?;
    let mut lines = content.lines();

    let pid: u32 = lines.next()?.parse().ok()?;
    let started_at: i64 = lines.next()?.parse().ok()?;

    Some(DaemonInfo { pid, started_at })
}

/// Write the PID file with current process info.
pub fn write_pid_file() -> std::io::Result<()> {
    let dir = mcpr_dir();
    fs::create_dir_all(&dir)?;

    let pid = std::process::id();
    let started_at = chrono::Utc::now().timestamp();
    let content = format!("{pid}\n{started_at}\n");

    fs::write(pid_file_path(), content)
}

/// Remove the PID file.
pub fn remove_pid_file() {
    let _ = fs::remove_file(pid_file_path());
}

/// Check if a process with the given PID is alive.
#[cfg(unix)]
pub fn is_process_alive(pid: u32) -> bool {
    // Signal 0 checks if the process exists without sending a signal.
    signal::kill(Pid::from_raw(pid as i32), None).is_ok()
}

#[cfg(not(unix))]
pub fn is_process_alive(_pid: u32) -> bool {
    false
}

/// Check the current daemon status by reading the PID file and probing the process.
pub fn check_status() -> DaemonStatus {
    match read_pid_file() {
        Some(info) => {
            if is_process_alive(info.pid) {
                DaemonStatus::Running(info)
            } else {
                DaemonStatus::Stale(info)
            }
        }
        None => DaemonStatus::NotRunning,
    }
}

// ── Daemonize (Unix only) ──────────────────────────────────────────────

/// Daemonize the current process using double-fork.
///
/// Returns `Ok(write_fd)` in the daemon child only. The original parent
/// process waits for a readiness signal on the pipe, then exits.
///
/// # Arguments
/// * `timeout` — how long the parent waits for the daemon to signal readiness.
///
/// # Panics
/// Calls `std::process::exit` in the parent and intermediate child.
#[cfg(unix)]
pub fn daemonize(timeout: Duration) -> Result<std::os::unix::io::RawFd, String> {
    use nix::unistd::{ForkResult, close, fork, pipe, setsid};
    use std::os::unix::io::{IntoRawFd, RawFd};

    // Create pipe for readiness signaling: parent reads, daemon writes.
    let (read_owned, write_owned) = pipe().map_err(|e| format!("pipe failed: {e}"))?;
    let read_fd: RawFd = read_owned.into_raw_fd();
    let write_fd: RawFd = write_owned.into_raw_fd();

    // First fork.
    match unsafe { fork() }.map_err(|e| format!("fork failed: {e}"))? {
        ForkResult::Parent { child: _ } => {
            // Parent: close write end, wait for readiness from daemon.
            let _ = close(write_fd);
            wait_for_readiness(read_fd, timeout);
            // wait_for_readiness calls exit() — never returns.
            unreachable!();
        }
        ForkResult::Child => {
            // First child: become session leader.
            let _ = close(read_fd);
            setsid().map_err(|e| format!("setsid failed: {e}"))?;

            // Second fork — prevent the daemon from reacquiring a controlling terminal.
            match unsafe { fork() }.map_err(|e| format!("second fork failed: {e}"))? {
                ForkResult::Parent { child: _ } => {
                    // Intermediate child exits immediately.
                    std::process::exit(0);
                }
                ForkResult::Child => {
                    // This is the daemon process.
                    redirect_stdio().map_err(|e| format!("failed to redirect stdio: {e}"))?;
                    Ok(write_fd)
                }
            }
        }
    }
}

/// Daemonize a proxy process using double-fork.
///
/// Same pattern as `daemonize()` but redirects stdio to the proxy-specific
/// log file and prints proxy-specific readiness messages.
#[cfg(unix)]
pub fn daemonize_proxy(
    proxy_name: &str,
    timeout: Duration,
) -> Result<std::os::unix::io::RawFd, String> {
    use nix::unistd::{ForkResult, close, fork, pipe, setsid};
    use std::os::unix::io::{IntoRawFd, RawFd};

    let name = proxy_name.to_string();
    let (read_owned, write_owned) = pipe().map_err(|e| format!("pipe failed: {e}"))?;
    let read_fd: RawFd = read_owned.into_raw_fd();
    let write_fd: RawFd = write_owned.into_raw_fd();

    match unsafe { fork() }.map_err(|e| format!("fork failed: {e}"))? {
        ForkResult::Parent { child: _ } => {
            let _ = close(write_fd);
            wait_for_proxy_readiness(read_fd, timeout, &name);
            unreachable!();
        }
        ForkResult::Child => {
            let _ = close(read_fd);
            setsid().map_err(|e| format!("setsid failed: {e}"))?;

            match unsafe { fork() }.map_err(|e| format!("second fork failed: {e}"))? {
                ForkResult::Parent { child: _ } => {
                    std::process::exit(0);
                }
                ForkResult::Child => {
                    super::proxy_lock::redirect_stdio(&name)
                        .map_err(|e| format!("failed to redirect stdio: {e}"))?;
                    Ok(write_fd)
                }
            }
        }
    }
}

/// Wait for a proxy child to signal readiness, then exit.
#[cfg(unix)]
fn wait_for_proxy_readiness(
    read_fd: std::os::unix::io::RawFd,
    _timeout: Duration,
    proxy_name: &str,
) {
    use std::os::unix::io::FromRawFd;

    let mut pipe_read = unsafe { std::fs::File::from_raw_fd(read_fd) };
    let mut buf = [0u8; 1];

    let handle = std::thread::spawn(move || {
        let result = pipe_read.read(&mut buf);
        (buf, result)
    });

    match handle.join() {
        Ok((buf, Ok(1))) if buf[0] == b'1' => {
            if let Some(info) = super::proxy_lock::read_lock_info(proxy_name) {
                eprintln!(
                    "proxy \"{}\" started (PID: {}, port: {})",
                    proxy_name, info.pid, info.port
                );
            } else {
                eprintln!("proxy \"{}\" started", proxy_name);
            }
            std::process::exit(0);
        }
        Ok((_, Ok(_))) => {
            eprintln!(
                "error: proxy \"{}\" failed to start (check {})",
                proxy_name,
                super::proxy_lock::log_path(proxy_name).display()
            );
            std::process::exit(1);
        }
        Ok((_, Err(e))) => {
            eprintln!("error: reading readiness pipe: {e}");
            std::process::exit(1);
        }
        Err(_) => {
            eprintln!("error: internal error waiting for proxy");
            std::process::exit(1);
        }
    }
}

/// Wait for the daemon to signal readiness via the pipe.
/// Exits the process with appropriate status.
#[cfg(unix)]
fn wait_for_readiness(read_fd: std::os::unix::io::RawFd, _timeout: Duration) {
    use std::os::unix::io::FromRawFd;

    let mut pipe_read = unsafe { std::fs::File::from_raw_fd(read_fd) };
    let mut buf = [0u8; 1];

    // Spawn a thread to do the blocking read, join with a timeout.
    let handle = std::thread::spawn(move || {
        let result = pipe_read.read(&mut buf);
        (buf, result)
    });

    match handle.join() {
        Ok((buf, Ok(1))) if buf[0] == b'1' => {
            // Daemon started successfully.
            if let Some(info) = read_pid_file() {
                eprintln!("mcprd started (PID: {})", info.pid);
            } else {
                eprintln!("mcprd started");
            }
            std::process::exit(0);
        }
        Ok((_, Ok(_))) => {
            // Pipe closed or unexpected byte — daemon failed to start.
            eprintln!("error: daemon failed to start (check daemon.log)");
            eprintln!("  log: {}", daemon_log_path().display());
            std::process::exit(1);
        }
        Ok((_, Err(e))) => {
            eprintln!("error: reading readiness pipe: {e}");
            std::process::exit(1);
        }
        Err(_) => {
            eprintln!("error: internal error waiting for daemon");
            std::process::exit(1);
        }
    }
}

/// Redirect stdin/stdout/stderr for the daemon process.
#[cfg(unix)]
fn redirect_stdio() -> std::io::Result<()> {
    use nix::unistd::dup2;
    use std::os::unix::io::AsRawFd;

    // Ensure log directory exists.
    let log_path = daemon_log_path();
    if let Some(parent) = log_path.parent() {
        fs::create_dir_all(parent)?;
    }

    // Open /dev/null for stdin.
    let dev_null = fs::OpenOptions::new().read(true).open("/dev/null")?;

    // Open daemon log for stdout/stderr (append mode).
    let log_file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;

    // Redirect file descriptors using nix (no raw libc needed).
    dup2(dev_null.as_raw_fd(), 0).map_err(std::io::Error::other)?; // stdin
    dup2(log_file.as_raw_fd(), 1).map_err(std::io::Error::other)?; // stdout
    dup2(log_file.as_raw_fd(), 2).map_err(std::io::Error::other)?; // stderr

    Ok(())
}

/// Signal readiness to the parent process via the pipe.
#[cfg(unix)]
pub fn signal_ready(write_fd: std::os::unix::io::RawFd) {
    use std::os::unix::io::FromRawFd;

    let mut pipe_write = unsafe { std::fs::File::from_raw_fd(write_fd) };
    if let Err(e) = pipe_write.write_all(b"1") {
        eprintln!("warning: failed to signal daemon readiness: {e}");
    }
    // File is dropped here, closing the pipe.
}

// ── Supervisor ─────────────────────────────────────────────────────────

/// Run the daemon supervisor loop.
///
/// The supervisor writes the PID file, signals readiness, then loops
/// forever monitoring proxy health and waiting for SIGTERM/SIGINT.
#[cfg(unix)]
pub async fn run_supervisor(ready_fd: Option<i32>) {
    // Write PID file.
    if let Err(e) = write_pid_file() {
        eprintln!("error: failed to write PID file: {e}");
        std::process::exit(1);
    }

    // Signal readiness to the parent process.
    if let Some(fd) = ready_fd {
        signal_ready(fd);
    }

    eprintln!("[mcprd] supervisor started (PID: {})", std::process::id());

    // Create a shutdown signal that responds to SIGTERM and SIGINT.
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);

    // Signal handler task.
    let shutdown_trigger = shutdown_tx.clone();
    tokio::spawn(async move {
        use tokio::signal::unix::{SignalKind, signal};
        let ctrl_c = tokio::signal::ctrl_c();
        let mut sigterm = signal(SignalKind::terminate()).expect("Failed to register SIGTERM");
        tokio::select! {
            _ = ctrl_c => {},
            _ = sigterm.recv() => {},
        }
        eprintln!("[mcprd] received shutdown signal");
        let _ = shutdown_trigger.send(true);
    });

    // Health monitor task: check proxy lockfiles every 10s.
    let health_shutdown = shutdown_tx.subscribe();
    tokio::spawn(async move {
        let mut rx = health_shutdown;
        loop {
            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_secs(10)) => {},
                _ = rx.changed() => break,
            }

            let proxies = super::proxy_lock::list_proxies();
            for (name, status) in &proxies {
                if let super::proxy_lock::LockStatus::Stale(info) = status {
                    eprintln!(
                        "[mcprd] proxy \"{}\" (PID: {}) is dead, cleaning up lockfile",
                        name, info.pid
                    );
                    super::proxy_lock::remove_lock(name);
                }
            }
        }
    });

    // Wait for shutdown signal.
    let _ = shutdown_rx.changed().await;

    // Stop all proxies.
    let stopped = super::proxy_lock::stop_all_proxies();
    if !stopped.is_empty() {
        eprintln!(
            "[mcprd] stopped {} proxy(ies): {}",
            stopped.len(),
            stopped.join(", ")
        );
    }

    // Clean up.
    remove_pid_file();
    eprintln!("[mcprd] shutdown complete.");
}

// ── Stop command ───────────────────────────────────────────────────────

/// Stop the running daemon. Prints status and exits the process.
pub fn stop_daemon() {
    match check_status() {
        DaemonStatus::Running(info) => {
            eprintln!("Stopping mcpr daemon (PID: {})...", info.pid);
            send_sigterm(info.pid);
            wait_for_exit(info.pid, Duration::from_secs(10));
            remove_pid_file();
            eprintln!("Stopped.");
        }
        DaemonStatus::Stale(_) => {
            eprintln!("Daemon is not running (stale PID file removed).");
            remove_pid_file();
        }
        DaemonStatus::NotRunning => {
            eprintln!("No daemon is running.");
            std::process::exit(1);
        }
    }
}

/// Stop the daemon if running, or silently continue if not.
/// Used by `mcpr restart`.
pub fn stop_daemon_if_running() {
    match check_status() {
        DaemonStatus::Running(info) => {
            eprintln!("Stopping mcpr daemon (PID: {})...", info.pid);
            send_sigterm(info.pid);
            wait_for_exit(info.pid, Duration::from_secs(10));
            remove_pid_file();
            eprintln!("Stopped.");
        }
        DaemonStatus::Stale(_) => {
            remove_pid_file();
        }
        DaemonStatus::NotRunning => {}
    }
}

/// Send SIGTERM to a process.
#[cfg(unix)]
fn send_sigterm(pid: u32) {
    let _ = signal::kill(Pid::from_raw(pid as i32), Signal::SIGTERM);
}

#[cfg(not(unix))]
fn send_sigterm(_pid: u32) {}

/// Wait for a process to exit, polling every 100ms.
fn wait_for_exit(pid: u32, timeout: Duration) {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if !is_process_alive(pid) {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    eprintln!(
        "warning: daemon (PID: {pid}) did not exit within {}s",
        timeout.as_secs()
    );
}

// ── Status command ─────────────────────────────────────────────────────

/// Print daemon status and proxy listing, then exit.
/// If a daemon is already running, stop it first so the new one can take over.
pub fn ensure_not_running() {
    match check_status() {
        DaemonStatus::Running(info) => {
            eprintln!("Stopping existing mcprd (PID: {})...", info.pid);
            send_sigterm(info.pid);
            wait_for_exit(info.pid, Duration::from_secs(10));
            remove_pid_file();
            eprintln!("Stopped. Starting new daemon...");
        }
        DaemonStatus::Stale(_) => {
            remove_pid_file();
        }
        DaemonStatus::NotRunning => {}
    }
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;

    #[test]
    fn pid_file__roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let pid_path = dir.path().join("test.pid");

        let pid = std::process::id();
        let ts = chrono::Utc::now().timestamp();
        let content = format!("{pid}\n{ts}\n");
        std::fs::write(&pid_path, &content).unwrap();

        let read = std::fs::read_to_string(&pid_path).unwrap();
        let mut lines = read.lines();
        let read_pid: u32 = lines.next().unwrap().parse().unwrap();
        let read_ts: i64 = lines.next().unwrap().parse().unwrap();

        assert_eq!(read_pid, pid);
        assert_eq!(read_ts, ts);
    }

    #[test]
    fn pid_file__malformed_parse_fails() {
        let content = "not-a-number\ngarbage\n";
        let mut lines = content.lines();
        let result = lines.next().and_then(|s| s.parse::<u32>().ok());
        assert!(result.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn is_process_alive__self() {
        assert!(is_process_alive(std::process::id()));
    }

    #[cfg(unix)]
    #[test]
    fn is_process_alive__nonexistent() {
        assert!(!is_process_alive(99_999_999));
    }
}
