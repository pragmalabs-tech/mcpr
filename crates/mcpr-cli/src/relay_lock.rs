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
    })
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;

    // ── read_lock_file ────────────────────────────────────────────────

    #[test]
    fn read_lock_file__roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("lock");

        let pid = std::process::id();
        let ts = chrono::Utc::now().timestamp();
        let content = format!("{pid}\n8080\n{ts}\n/tmp/relay.toml\n");
        fs::write(&lock, &content).unwrap();

        let info = read_lock_file(&lock).unwrap();
        assert_eq!(info.pid, pid);
        assert_eq!(info.port, 8080);
        assert_eq!(info.started_at, ts);
        assert_eq!(info.config_path, "/tmp/relay.toml");
    }

    #[test]
    fn read_lock_file__missing_returns_none() {
        let info = read_lock_file(Path::new("/nonexistent/path/lock"));
        assert!(info.is_none());
    }

    #[test]
    fn read_lock_file__malformed_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("lock");
        fs::write(&lock, "12345\n8080\n").unwrap();
        assert!(read_lock_file(&lock).is_none());
    }

    #[test]
    fn read_lock_file__empty_file_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("lock");
        fs::write(&lock, "").unwrap();
        assert!(read_lock_file(&lock).is_none());
    }

    #[test]
    fn read_lock_file__non_numeric_pid_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("lock");
        fs::write(&lock, "notapid\n8080\n12345\n/tmp/relay.toml\n").unwrap();
        assert!(read_lock_file(&lock).is_none());
    }

    #[test]
    fn read_lock_file__non_numeric_port_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("lock");
        fs::write(&lock, "1234\nnotaport\n12345\n/tmp/relay.toml\n").unwrap();
        assert!(read_lock_file(&lock).is_none());
    }

    // ── path helpers ──────────────────────────────────────────────────

    #[test]
    fn relay_dir__under_mcpr() {
        let d = relay_dir();
        assert!(d.ends_with(".mcpr/relay"));
    }

    #[test]
    fn lock_path__under_relay_dir() {
        let p = lock_path();
        assert!(p.ends_with("relay/lock"));
    }

    // ── Integration tests using real ~/.mcpr/relay/ paths ──────────────
    //
    // These tests MUST run sequentially since they share the same lockfile.
    // We combine them into one test to avoid parallel conflicts.

    #[cfg(unix)]
    #[test]
    fn real_path__lifecycle_roundtrip() {
        // 1. Start clean.
        remove_lock();

        // 2. check_lock returns Free when no lockfile.
        assert!(matches!(check_lock(), LockStatus::Free));

        // 3. stop_relay returns false when free.
        assert!(!stop_relay());

        // 4. Write a lockfile with our PID → Held.
        write_lock(9998, "/tmp/test-relay.toml").unwrap();
        assert!(matches!(check_lock(), LockStatus::Held(_)));

        // 5. Remove → Free again.
        remove_lock();
        assert!(matches!(check_lock(), LockStatus::Free));

        // 6. Write a stale lockfile (dead PID) → Stale.
        let dir = relay_dir();
        fs::create_dir_all(&dir).unwrap();
        let ts = chrono::Utc::now().timestamp();
        fs::write(
            lock_path(),
            format!("99999999\n8080\n{ts}\n/tmp/relay.toml\n"),
        )
        .unwrap();
        assert!(matches!(check_lock(), LockStatus::Stale(_)));

        // 7. stop_relay cleans stale lock.
        let result = stop_relay();
        assert!(!result); // returns false for stale
        assert!(matches!(check_lock(), LockStatus::Free));
    }

    // ── read_lock_file edge cases (temp dir) ─────────────────────────

    #[cfg(unix)]
    #[test]
    fn read_lock_file__stale_when_pid_dead() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("lock");

        let ts = chrono::Utc::now().timestamp();
        let content = format!("99999999\n8080\n{ts}\n/tmp/relay.toml\n");
        fs::write(&lock, &content).unwrap();

        let info = read_lock_file(&lock).unwrap();
        assert_eq!(info.pid, 99999999);
        assert!(!crate::proxy_lock::is_process_alive(99999999));
    }

    #[cfg(unix)]
    #[test]
    fn read_lock_file__held_when_pid_alive() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("lock");

        let pid = std::process::id();
        let ts = chrono::Utc::now().timestamp();
        let content = format!("{pid}\n8080\n{ts}\n/tmp/relay.toml\n");
        fs::write(&lock, &content).unwrap();

        let info = read_lock_file(&lock).unwrap();
        assert!(crate::proxy_lock::is_process_alive(info.pid));
    }

    #[test]
    fn read_lock_file__overflow_port_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("lock");
        fs::write(&lock, "1234\n99999\n12345\n/tmp/relay.toml\n").unwrap();
        assert!(read_lock_file(&lock).is_none()); // 99999 > u16::MAX
    }
}
