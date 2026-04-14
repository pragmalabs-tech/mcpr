# Architecture: mcprd + Proxy + Relay

mcpr uses a multi-process model inspired by Docker's daemon/CLI split.

## Overview

```
mcpr start           → starts mcprd (supervisor only, no config needed)
mcpr proxy run X     → snapshots config → starts proxy from snapshot
mcpr relay start X   → snapshots config → starts relay from snapshot
mcpr stop            → mcprd kills all proxies + relay → exits
mcpr restart         → collects running names → stop → start → re-launch all
daemon dies          → proxies and relay detect within 5s → self-terminate
```

**mcprd** (the daemon) is a pure supervisor. It has no MCP connection, no HTTP server, and needs no config file. It monitors proxy and relay health and manages their lifecycle.

**Proxies** are standalone processes. Each one loads its own config, binds its own port, and connects to its own upstream MCP server. If the daemon dies, proxies self-terminate within 5 seconds.

**Relay** is a singleton tunnel server. It accepts WebSocket connections from remote mcpr clients, assigns subdomains, and proxies HTTP requests through the tunnel. One relay per machine. If the daemon dies, the relay self-terminates within 5 seconds.

## `~/.mcpr/` — Central State Directory

All mcpr state lives under `~/.mcpr/`. No files in `~/Library/Application Support/`, `~/.local/share/`, or any other platform-specific location.

```
~/.mcpr/
├── mcprd.pid              # daemon PID + start timestamp
├── mcprd.log              # daemon stdout/stderr
├── store.db               # SQLite — request logs, sessions, schema
├── proxies/
│   └── {name}/
│       ├── config.toml    # snapshot of config at launch time (immutable)
│       ├── lock           # PID, port, timestamp, daemon_pid
│       └── proxy.log      # proxy stdout/stderr
└── relay/
    ├── config.toml        # snapshot of relay config at launch time
    ├── lock               # PID, port, timestamp
    └── relay.log          # relay stdout/stderr
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

Daemon stdout/stderr. Logs are minimal — startup messages, proxy/relay health monitoring, and shutdown events.

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

#### `relay/config.toml`

Snapshot of the relay config file at the time `mcpr relay start` was called. `mcpr relay restart` re-launches from this snapshot.

#### `relay/lock`

Four lines, plain text:

```
<pid>
<port>
<start_unix_timestamp>
<config_path>
```

Written after the relay binds its port. Removed on clean shutdown. No `daemon_pid` line — the relay reads the daemon PID from `mcprd.pid` at startup for its watchdog.

#### `relay/relay.log`

Relay stdout/stderr. Contains tunnel registration events and nginx-style access logs for proxied requests.

## Lifecycle

### Starting

```bash
mcpr start                    # fork mcprd supervisor, exit
mcpr proxy run search.toml    # snapshot config, fork proxy, exit
mcpr proxy run api.toml       # another proxy, same daemon
mcpr relay start relay.toml   # snapshot config, fork relay, exit
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

mcpr relay status
# Relay is running.
#   PID:     12348
#   Port:    8080
#   Uptime:  8100s
```

### Stopping

```bash
mcpr proxy stop search        # stop one proxy
mcpr proxy stop --all         # stop all proxies
mcpr relay stop               # stop the relay
mcpr stop                     # stop all proxies + relay + daemon
```

### Daemon Watchdog

Every 5 seconds, each proxy and the relay check if the daemon PID is still alive. If the daemon dies (crash, `kill -9`, etc.), all managed processes shut down gracefully within 5 seconds. This prevents orphaned processes.

### Daemon Health Monitor

Every 10 seconds, the daemon scans lockfiles under `~/.mcpr/proxies/` and `~/.mcpr/relay/`. If a process has died but left a stale lockfile, the daemon cleans it up and logs a warning.

### Restart

```bash
mcpr restart
```

This:
1. Collects the names of currently running proxies and whether the relay is running.
2. Stops all proxies and the relay (SIGTERM + wait).
3. Stops the daemon.
4. Forks a new daemon.
5. Re-launches the previously running proxies and relay from their saved config snapshots.

## Relay

The relay is a singleton — one per machine. It has its own command group (`mcpr relay`) separate from proxies.

| Command | Behavior |
|---------|----------|
| `mcpr relay run <config>` | Run in foreground. Does not require daemon. |
| `mcpr relay start <config>` | Snapshot config, fork to background. Requires daemon. |
| `mcpr relay stop` | SIGTERM + wait + remove lockfile. |
| `mcpr relay restart` | Stop + start from saved snapshot. |
| `mcpr relay restart <config>` | Stop + start with new config. |
| `mcpr relay status` | Show PID, port, uptime. |

`mcpr relay run` (foreground) does not require a running daemon. This mode is for Docker containers, systemd units, and debugging.

`mcpr relay start` (background) requires a running daemon and participates in daemon lifecycle — watchdog, health monitoring, and restart.

Relay config does not need `mode = "relay"` when using `mcpr relay` commands. The mode is implicit.

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

$ mcpr relay start relay.toml
error: daemon not running — run `mcpr start` first
```

Background proxies and relay require a running daemon. Start the daemon first with `mcpr start`. Foreground relay (`mcpr relay run`) does not have this requirement.
