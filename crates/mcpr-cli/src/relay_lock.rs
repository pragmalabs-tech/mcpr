//! Singleton relay lockfile management.
//!
//! The relay gets a single directory at `~/.mcpr/relay/` containing:
//! - `lock` — PID, port, timestamp, config path
//! - `config.toml` — snapshot of the config at launch time (used for restart)
//! - `relay.log` — stdout/stderr redirect for the background process

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub use crate::proxy_lock::{LockInfo, LockStatus};

// ── Path helpers ──────────────────────────────────────────────────────

fn relay_dir() -> PathBuf {
    crate::proxy_lock::mcpr_dir().join("relay")
}

fn lock_path() -> PathBuf {
    relay_dir().join("lock")
}

/// Path to the config snapshot for the relay.
pub fn config_snapshot_path() -> PathBuf {
    relay_dir().join("config.toml")
}

/// Path to the log file for the relay.
pub fn log_path() -> PathBuf {
    relay_dir().join("relay.log")
}

// ── Lock operations ──────────────────────────────────────────────────

/// Check the lock status for the relay.
pub fn check_lock() -> LockStatus {
    let path = lock_path();
    let info = match read_lock_file(&path) {
        Some(info) => info,
        None => return LockStatus::Free,
    };

    if crate::proxy_lock::is_process_alive(info.pid) {
        LockStatus::Held(info)
    } else {
        LockStatus::Stale(info)
    }
}

/// Read lock info for the relay.
pub fn read_lock_info() -> Option<LockInfo> {
    read_lock_file(&lock_path())
}

/// Write a lockfile for the relay after successful port bind.
pub fn write_lock(port: u16, config_path: &str) -> std::io::Result<()> {
    let dir = relay_dir();
    fs::create_dir_all(&dir)?;

    let pid = std::process::id();
    let started_at = chrono::Utc::now().timestamp();
    let content = format!("{pid}\n{port}\n{started_at}\n{config_path}\n");

    fs::write(lock_path(), content)
}

/// Remove the relay lockfile.
pub fn remove_lock() {
    let _ = fs::remove_file(lock_path());
}

/// Save a config snapshot for the relay (used for restart).
pub fn snapshot_config(content: &str) -> std::io::Result<()> {
    let dir = relay_dir();
    fs::create_dir_all(&dir)?;
    fs::write(config_snapshot_path(), content)
}

/// Read the saved config snapshot for the relay.
pub fn read_snapshot() -> std::io::Result<String> {
    fs::read_to_string(config_snapshot_path())
}

/// Stop the running relay: send SIGTERM, wait, remove lock.
/// Returns `true` if the process was stopped, `false` if it wasn't running.
pub fn stop_relay() -> bool {
    match check_lock() {
        LockStatus::Held(info) => {
            crate::proxy_lock::send_sigterm(info.pid);
            crate::proxy_lock::wait_for_exit(info.pid, Duration::from_secs(10));
            remove_lock();
            true
        }
        LockStatus::Stale(_) => {
            remove_lock();
            false
        }
        LockStatus::Free => false,
    }
}

// ── Stdio redirect ──────────────────────────────────────────────────

/// Redirect stdin/stdout/stderr for the background relay process.
#[cfg(unix)]
pub fn redirect_stdio() -> std::io::Result<()> {
    use nix::unistd::dup2;
    use std::os::unix::io::AsRawFd;

    let log = log_path();
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

// ── Lock file parser ────────────────────────────────────────────────

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
        daemon_pid: None,
    })
}
