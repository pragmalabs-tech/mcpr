//! Per-process backgrounding helpers — fork into the background and prove
//! the child is alive via a pipe handshake.
//!
//! `mcpr proxy run --background` and `mcpr relay run --background` go through
//! `daemonize_proxy` / `daemonize_relay`. The child writes `b"1"` into the
//! pipe (`signal_ready`) once it has bound its listener and written its
//! lockfile; the parent blocks in `wait_for_*_readiness`, prints a
//! human-readable startup line, then exits 0. Parent failure modes (child
//! exited before signaling, pipe closed early, read error) all surface as
//! exit 1 with a hint to the proxy/relay log.
//!
//! Foreground mode is the default — none of this code runs there. The
//! parent process owns the PID directly (systemd, Node `child_process.spawn`,
//! `pm2`, …), which is what host MCP servers want when wrapping mcpr.

#[cfg(unix)]
use std::io::{Read, Write};
#[cfg(unix)]
use std::time::Duration;

// ── Daemonize a proxy or relay (Unix only) ───────────────────────────────

/// Double-fork the current process into the background as a proxy.
///
/// Returns `Ok(write_fd)` in the grandchild only. The original parent
/// blocks in `wait_for_proxy_readiness` and exits when the grandchild
/// signals readiness.
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

/// Double-fork the current process into the background as the relay.
#[cfg(unix)]
pub fn daemonize_relay(timeout: Duration) -> Result<std::os::unix::io::RawFd, String> {
    use nix::unistd::{ForkResult, close, fork, pipe, setsid};
    use std::os::unix::io::{IntoRawFd, RawFd};

    let (read_owned, write_owned) = pipe().map_err(|e| format!("pipe failed: {e}"))?;
    let read_fd: RawFd = read_owned.into_raw_fd();
    let write_fd: RawFd = write_owned.into_raw_fd();

    match unsafe { fork() }.map_err(|e| format!("fork failed: {e}"))? {
        ForkResult::Parent { child: _ } => {
            let _ = close(write_fd);
            wait_for_relay_readiness(read_fd, timeout);
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
                    super::relay_lock::redirect_stdio()
                        .map_err(|e| format!("failed to redirect stdio: {e}"))?;
                    Ok(write_fd)
                }
            }
        }
    }
}

// ── Pipe handshake — child → parent ──────────────────────────────────────

/// Signal readiness to the parent process via the pipe.
#[cfg(unix)]
pub fn signal_ready(write_fd: std::os::unix::io::RawFd) {
    use std::os::unix::io::FromRawFd;

    let mut pipe_write = unsafe { std::fs::File::from_raw_fd(write_fd) };
    if let Err(e) = pipe_write.write_all(b"1") {
        eprintln!("warning: failed to signal readiness: {e}");
    }
    // File is dropped here, closing the pipe.
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
                // Show tunnel and dashboard URLs from config snapshot.
                if let Ok(snapshot) = super::proxy_lock::read_snapshot(proxy_name)
                    && let Ok(table) = snapshot.parse::<toml::Table>()
                {
                    if let Some(subdomain) = table
                        .get("tunnel")
                        .and_then(|t| t.as_table())
                        .filter(|t| t.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false))
                        .and_then(|t| t.get("subdomain"))
                        .and_then(|v| v.as_str())
                    {
                        eprintln!("  tunnel:    https://{subdomain}.tunnel.mcpr.app");
                    }
                    if let Some(server) = table
                        .get("cloud")
                        .and_then(|t| t.as_table())
                        .and_then(|t| t.get("server"))
                        .and_then(|v| v.as_str())
                    {
                        eprintln!("  dashboard: https://cloud.mcpr.app/servers/{server}");
                    }
                }
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

/// Wait for the relay child to signal readiness, then exit.
#[cfg(unix)]
fn wait_for_relay_readiness(read_fd: std::os::unix::io::RawFd, _timeout: Duration) {
    use std::os::unix::io::FromRawFd;

    let mut pipe_read = unsafe { std::fs::File::from_raw_fd(read_fd) };
    let mut buf = [0u8; 1];

    let handle = std::thread::spawn(move || {
        let result = pipe_read.read(&mut buf);
        (buf, result)
    });

    match handle.join() {
        Ok((buf, Ok(1))) if buf[0] == b'1' => {
            if let Some(info) = super::relay_lock::read_lock_info() {
                eprintln!("relay started (PID: {}, port: {})", info.pid, info.port);
            } else {
                eprintln!("relay started");
            }
            std::process::exit(0);
        }
        Ok((_, Ok(_))) => {
            eprintln!(
                "error: relay failed to start (check {})",
                super::relay_lock::log_path().display()
            );
            std::process::exit(1);
        }
        Ok((_, Err(e))) => {
            eprintln!("error: reading readiness pipe: {e}");
            std::process::exit(1);
        }
        Err(_) => {
            eprintln!("error: internal error waiting for relay");
            std::process::exit(1);
        }
    }
}
