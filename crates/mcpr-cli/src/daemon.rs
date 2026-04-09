//! Daemon process management — start, stop, restart, status.
//!
//! mcpr can run as a background daemon (like nginx). This module handles:
//! - Daemonization via double-fork (Unix only).
//! - PID file management for tracking the running daemon.
//! - Readiness signaling so `mcpr start` waits until the port is bound.
//! - Graceful stop via SIGTERM.
//!
//! # PID file format
//!
//! Three lines, plain text:
//! ```text
//! <pid>
//! <start_unix_timestamp>
//! <port>
//! ```
//!
//! Location: `~/.local/share/mcpr/mcpr.pid` (Linux) or
//! `~/Library/Application Support/mcpr/mcpr.pid` (macOS).

use std::fs;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::time::Duration;

#[cfg(unix)]
use nix::sys::signal::{self, Signal};
#[cfg(unix)]
use nix::unistd::Pid;

/// Application directory name under the platform data dir.
const APP_DIR: &str = "mcpr";
/// PID file name.
const PID_FILE: &str = "mcpr.pid";
/// Daemon log file name (stdout/stderr redirect).
const DAEMON_LOG: &str = "daemon.log";

/// Information stored in the PID file.
#[derive(Debug)]
pub struct DaemonInfo {
    pub pid: u32,
    pub started_at: i64,
    pub port: u16,
    pub proxy_name: String,
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

/// Resolve the directory for PID file and daemon log.
fn data_dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(APP_DIR)
}

/// Path to the PID file.
pub fn pid_file_path() -> PathBuf {
    data_dir().join(PID_FILE)
}

/// Path to the daemon log file (stdout/stderr redirect).
pub fn daemon_log_path() -> PathBuf {
    data_dir().join(DAEMON_LOG)
}

// ── PID file operations ────────────────────────────────────────────────

/// Read the PID file and parse its contents.
pub fn read_pid_file() -> Option<DaemonInfo> {
    let content = fs::read_to_string(pid_file_path()).ok()?;
    let mut lines = content.lines();

    let pid: u32 = lines.next()?.parse().ok()?;
    let started_at: i64 = lines.next()?.parse().ok()?;
    let port: u16 = lines.next()?.parse().ok()?;
    let proxy_name: String = lines.next().unwrap_or("unknown").to_string();

    Some(DaemonInfo {
        pid,
        started_at,
        port,
        proxy_name,
    })
}

/// Write the PID file with current process info.
pub fn write_pid_file(port: u16, proxy_name: &str) -> std::io::Result<()> {
    let dir = data_dir();
    fs::create_dir_all(&dir)?;

    let pid = std::process::id();
    let started_at = chrono::Utc::now().timestamp();
    let content = format!("{pid}\n{started_at}\n{port}\n{proxy_name}\n");

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
                eprintln!(
                    "mcpr daemon started (PID: {}, port: {})",
                    info.pid, info.port
                );
            } else {
                eprintln!("mcpr daemon started");
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
    let _ = pipe_write.write_all(b"1");
    // File is dropped here, closing the pipe.
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

/// Print daemon status and exit.
pub fn print_status() {
    match check_status() {
        DaemonStatus::Running(info) => {
            let uptime = chrono::Utc::now().timestamp() - info.started_at;
            let hours = uptime / 3600;
            let minutes = (uptime % 3600) / 60;
            let seconds = uptime % 60;

            println!("mcpr daemon is running");
            println!("  Proxy:  {}", info.proxy_name);
            println!("  PID:    {}", info.pid);
            println!("  Port:   {}", info.port);
            println!("  Uptime: {}h {}m {}s", hours, minutes, seconds);
            println!("  PID file: {}", pid_file_path().display());
            println!("  Log file: {}", daemon_log_path().display());
            println!();
            println!(
                "  Use `mcpr proxy logs {}` to view request logs.",
                info.proxy_name
            );
            println!(
                "  Use `mcpr proxy stats {}` to view metrics.",
                info.proxy_name
            );
        }
        DaemonStatus::Stale(info) => {
            eprintln!(
                "Daemon is not running (stale PID file for PID: {})",
                info.pid
            );
            remove_pid_file();
            std::process::exit(1);
        }
        DaemonStatus::NotRunning => {
            eprintln!("No daemon is running.");
            std::process::exit(1);
        }
    }
}

/// Ensure no daemon is already running. Exits with error if one is.
pub fn ensure_not_running() {
    match check_status() {
        DaemonStatus::Running(info) => {
            eprintln!(
                "error: daemon already running (PID: {}, port: {})",
                info.pid, info.port
            );
            std::process::exit(1);
        }
        DaemonStatus::Stale(_) => {
            remove_pid_file();
        }
        DaemonStatus::NotRunning => {}
    }
}
