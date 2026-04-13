# Architecture: mcprd + Proxy

mcpr uses a two-process model inspired by Docker's daemon/CLI split.

## Overview

```
mcpr start           → starts mcprd (supervisor only, no config needed)
mcpr proxy run X     → snapshots config → starts proxy from snapshot
mcpr stop            → mcprd kills all proxies → exits
mcpr restart         → collects proxy names → stop → start → re-launch proxies
daemon dies          → proxies detect within 5s → self-terminate
```

**mcprd** (the daemon) is a pure supervisor. It has no MCP connection, no HTTP server, and needs no config file. It monitors proxy health and manages their lifecycle.

**Proxies** are standalone processes. Each one loads its own config, binds its own port, and connects to its own upstream MCP server. If the daemon dies, proxies self-terminate within 5 seconds.

## `~/.mcpr/` — Central State Directory

All mcpr state lives under `~/.mcpr/`. No files in `~/Library/Application Support/`, `~/.local/share/`, or any other platform-specific location.

```
~/.mcpr/
├── mcprd.pid              # daemon PID + start timestamp
├── mcprd.log              # daemon stdout/stderr
├── store.db               # SQLite — request logs, sessions, schema
└── proxies/
    └── {name}/
        ├── config.toml    # snapshot of config at launch time (immutable)
        ├── lock           # PID, port, timestamp, daemon_pid
        └── proxy.log      # proxy stdout/stderr
```

### File Details

#### `mcprd.pid`

Two lines, plain text:

```
<pid>
<start_unix_timestamp>
```

Written by `mcpr start`. Removed on clean shutdown. If the daemon crashes, the stale PID file is detected and cleaned up on the next `mcpr start`.

#### `mcprd.log`

Daemon stdout/stderr. The daemon logs are minimal — just startup messages, proxy health monitoring, and shutdown events.

#### `store.db`

SQLite database shared by all proxies. Stores request logs, session data, schema snapshots, and heartbeat events. Queried by `mcpr proxy logs`, `mcpr proxy stats`, and other observability commands. These commands work even when the daemon isn't running.

Override with `MCPR_DB=/path/to/db` environment variable.

#### `proxies/{name}/config.toml`

Snapshot of the `mcpr.toml` file at the time `mcpr proxy run` was called. The proxy always runs from this snapshot, not the original file. This means:

- Editing the original `mcpr.toml` doesn't affect running proxies.
- `mcpr proxy restart <name>` re-launches from the snapshot.
- `mcpr proxy run <config> --replace` creates a new snapshot.

#### `proxies/{name}/lock`

Five lines, plain text:

```
<pid>
<port>
<start_unix_timestamp>
<config_path>
<daemon_pid>
```

Written after the proxy binds its port. Removed on clean shutdown. The `daemon_pid` line records which daemon was running when the proxy started — the proxy uses this for the watchdog check.

#### `proxies/{name}/proxy.log`

Proxy stdout/stderr. Contains structured log output (JSON by default) for all MCP requests flowing through this proxy.

## Lifecycle

### Starting

```bash
mcpr start                    # fork mcprd supervisor, exit
mcpr proxy run search.toml    # snapshot config, fork proxy, exit
mcpr proxy run api.toml       # another proxy, same daemon
```

### Status

```bash
mcpr status
# mcprd running (PID: 12345, uptime: 2h 15m 30s)
#   PID file: ~/.mcpr/mcprd.pid
#   Log file: ~/.mcpr/mcprd.log
#
#   2 proxy(ies) running:
#     search (PID: 12346, port: 3000)
#     api (PID: 12347, port: 3001)
```

### Stopping

```bash
mcpr proxy stop search        # stop one proxy
mcpr proxy stop --all         # stop all proxies
mcpr stop                     # stop all proxies + daemon
```

### Daemon Watchdog

Every 5 seconds, each proxy checks if the daemon PID is still alive. If the daemon dies (crash, `kill -9`, etc.), all proxies shut down gracefully within 5 seconds. This prevents orphaned proxy processes.

### Daemon Health Monitor

Every 10 seconds, the daemon scans proxy lockfiles under `~/.mcpr/proxies/`. If a proxy process has died but left a stale lockfile, the daemon cleans it up and logs a warning.

### Restart

```bash
mcpr restart
```

This:
1. Collects the names of currently running proxies.
2. Stops all proxies (SIGTERM + wait).
3. Stops the daemon.
4. Forks a new daemon.
5. Re-launches the previously running proxies from their saved config snapshots.

## Proxy Name Resolution

The proxy name is derived from (in order):

1. `name = "my-proxy"` in `mcpr.toml`
2. The config filename stem (e.g., `search.toml` → `search`)
3. `"default"` if the config is `mcpr.toml` or no file is specified

Characters that aren't alphanumeric or `-` are replaced with `-`.

## Error: Daemon Not Running

```bash
$ mcpr proxy run search.toml
error: daemon not running — run `mcpr start` first
```

Proxies require a running daemon. Start the daemon first with `mcpr start`.
