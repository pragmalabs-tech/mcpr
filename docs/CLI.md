# CLI Reference

The CLI runs proxies and maintains local storage. It does not configure proxy behavior - that's [`mcpr.toml`](proxy/PROXY_CONFIGURATION.md).

mcpr is a sidecar primitive in the envoy / pgbouncer mold: a host process (your Node / Go / Ruby MCP server, systemd, Docker) spawns `mcpr proxy run <config>` as a child and supervises it directly. The PID you launch is the proxy itself, so SIGTERM drains it gracefully, crash-then-restart loops Just Work, and the host supervisor (`docker stop`, `systemctl status`, `kubectl`) is the source of truth for which proxies are running.

> Running in a container? The [Docker image](DOCKER.md) execs `mcpr proxy run` directly as PID 1, so `docker stop` translates straight to a graceful SIGTERM.

## Quick Start

```bash
# Run a proxy (foreground; Ctrl-C / SIGTERM to stop)
mcpr proxy run mcpr.toml
```

To stop, kill the process via your supervisor (Ctrl-C from the terminal, `docker stop`, `systemctl stop`, `kill -TERM <pid>`).

## Commands

### `mcpr proxy run [CONFIG]`

Run a proxy from a config file in the foreground. The launched PID is the proxy itself. Your process supervisor (terminal, systemd, Docker, Node `child_process.spawn`) owns the lifecycle. SIGTERM drains gracefully (up to `drain_timeout`, default 30s).

```bash
mcpr proxy run mcpr.toml
```

| Argument | Description |
|----------|-------------|
| `[CONFIG]` | Config file path (default: `./mcpr.toml`) |

Restart is the host supervisor's job (systemd `Restart=on-failure`, Docker `restart=always`, k8s `restartPolicy`).

### `mcpr proxy setup`

Interactive setup that connects your proxy to mcpr Cloud. Authenticates via email, lets you pick or create a project and server, generates a project token, and writes `mcpr.toml`.

```bash
mcpr proxy setup                         # writes ./mcpr.toml
mcpr proxy setup -o /path/to/mcpr.toml   # write to a specific path
```

The setup flow:
1. Sends a login code to your email
2. Authenticates with mcpr Cloud
3. Lets you select or create a project
4. Lets you select or create a server
5. Generates a project token
6. Writes `mcpr.toml` with `[cloud]` config filled in

If `mcpr.toml` already exists, setup reads existing values as defaults.

| Flag | Description |
|------|-------------|
| `-o, --output PATH` | Output config path (default: `./mcpr.toml`) |
| `--cloud-url URL` | Cloud API base URL (default: `https://api.mcpr.app`) |

### `mcpr store stats`

Show database size, row counts, and record age for the local SQLite store at `~/.mcpr/store.db`.

```bash
mcpr store stats
```

Output:
```
STORAGE - /Users/you/Library/Application Support/mcpr/mcpr.db

  Total requests:    1,284,847
  Total sessions:    4,201
  Proxies tracked:   1
  Oldest record:     2026-03-01 08:12:44
  Newest record:     2026-04-06 10:18:33

  Database file:     847.0 MB
  WAL file:          2.1 MB
```

### `mcpr store vacuum --before DURATION`

Delete old records and reclaim disk space.

```bash
mcpr store vacuum --before 7d                          # delete records older than 7 days
mcpr store vacuum --before 30d --dry-run               # preview what would be deleted
mcpr store vacuum --before 7d --proxy localhost-9000   # scope to one proxy
```

| Flag | Description |
|------|-------------|
| `--before DURATION` | Delete records older than this (required) |
| `--proxy NAME` | Scope to one proxy |
| `--dry-run` | Preview without deleting |

### `mcpr validate`

Validate `mcpr.toml` and exit.

```bash
mcpr validate                       # validates ./mcpr.toml
mcpr validate -c path/to/mcpr.toml  # validate a specific file
mcpr validate --dump                # also print the resolved config to stdout
```

### `mcpr version`

Print version information.

## Duration Format

`--before` flags accept human-friendly durations:

| Suffix | Meaning | Example |
|--------|---------|---------|
| `s` | seconds | `30s` |
| `m` | minutes | `15m` |
| `h` | hours | `2h` |
| `d` | days | `7d` |
| `w` | weeks | `2w` |

## Proxy Name

The proxy name is derived from (in order):

1. `name = "my-proxy"` in `mcpr.toml`
2. The config filename stem (e.g., `search.toml` -> `search`)
3. `"default"` if the config is `mcpr.toml` or unspecified

## Files

| File | Purpose |
|------|---------|
| `~/.mcpr/store.db` | Request and event storage (SQLite) |
| `~/.mcpr/auth.json` | Cloud auth token written by `mcpr proxy setup` |

`mcpr proxy run` is stateless: it owns no on-disk lockfile or snapshot. The host supervisor (Docker / systemd / k8s) tracks running proxies; if you launch two proxies with the same port, the second exits when `bind()` returns `EADDRINUSE`.
