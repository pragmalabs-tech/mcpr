# CLI Reference

The CLI **manages the daemon, proxy, and relay processes** and **extracts information** from the local SQLite store. It does not configure proxy behavior — that's [`mcpr.toml`](proxy/PROXY_CONFIGURATION.md). See [ARCHITECTURE.md](ARCHITECTURE.md) for the full multi-process model.

> Running in a container? The [Docker image](DOCKER.md) wraps the daemon + proxy flow behind a single `docker run` command. The CLI commands below still apply inside the container via `docker exec`.

Three responsibilities:
1. **Lifecycle** — start/stop the daemon supervisor, individual proxies, and the relay server.
2. **Query & observe** — read request logs, per-tool metrics, sessions, schema, and storage stats from SQLite. These commands work even when the daemon isn't running.
3. **Relay** — run a tunnel relay server that accepts remote mcpr client connections.

## Quick Start

```bash
# Start the daemon supervisor
mcpr start

# Launch a proxy
mcpr proxy run mcpr.toml

# Launch a relay server
mcpr relay start relay.toml

# Check status
mcpr status
mcpr relay status

# View request logs
mcpr proxy logs

# Stop everything (proxies + relay + daemon)
mcpr stop
```

## Commands

### Daemon Lifecycle

| Command | Description |
|---------|-------------|
| `mcpr start` | Start the daemon supervisor (no config needed) |
| `mcpr start --foreground` | Start in foreground (for Docker, systemd, debugging) |
| `mcpr stop` | Stop all proxies + relay + daemon (graceful SIGTERM) |
| `mcpr restart` | Stop + start, re-launching previously running proxies and relay |
| `mcpr status` | Show daemon PID/uptime + list all proxies |

`mcpr start` launches the daemon supervisor and exits. No config file is needed — the daemon is a pure supervisor that monitors proxy and relay health. Logs go to `~/.mcpr/mcprd.log`.

```bash
mcpr start              # start daemon, no config needed
mcpr proxy run app.toml # then launch proxies
mcpr relay start relay.toml # optionally launch relay
```

### Proxy Lifecycle

#### `mcpr proxy list`

List all known proxies and their status (running, stale, stopped).

```bash
mcpr proxy list                          # table output
mcpr proxy list --json                   # JSON array
```

Output:
```
NAME                     STATUS        PID    PORT  STARTED               CONFIG
localhost-9000           running      1234    3000  2026-04-12 14:30:00   /home/you/api/mcpr.toml
staging-server           stale        5678    3001  2026-04-10 09:15:00   /home/you/staging/mcpr.toml

1 running, 2 total
```

| Flag | Default | Description |
|------|---------|-------------|
| `--json` | false | Output as a JSON array |

#### `mcpr proxy start <name>`

Start a stopped proxy by name, using its saved config snapshot. Errors if the proxy is already running or has no saved config.

```bash
mcpr proxy start localhost-9000
```

#### `mcpr proxy run`

Launch a proxy in the background from a config file. Snapshots the config to `~/.mcpr/proxies/<name>/config.toml` for later `start` / `restart` / `reload`. Errors if a proxy with the same name is already running — use `restart` to replace it or `reload` to update config without dropping sessions.

```bash
mcpr proxy run mcpr.toml
```

| Argument | Description |
|----------|-------------|
| `[CONFIG]` | Config file path (default: `./mcpr.toml`) |

#### `mcpr proxy stop [name]`

Stop a running proxy (SIGTERM, waits up to 10s for drain). Use `--all` to stop every running proxy.

```bash
mcpr proxy stop localhost-9000
mcpr proxy stop --all
```

#### `mcpr proxy restart [name]`

Process-level restart: stop the existing proxy and respawn it. In-flight MCP sessions are **dropped**. Pass `--config <path>` to apply a new config during the restart — the snapshot is refreshed from the given file before respawn. Without `--config`, the existing snapshot is reused.

```bash
mcpr proxy restart localhost-9000                        # reuse saved snapshot
mcpr proxy restart localhost-9000 -c mcpr.toml           # apply new config
mcpr proxy restart --all                                 # restart every proxy
```

| Flag | Description |
|------|-------------|
| `-c, --config PATH` | Refresh the snapshot from this file before respawn |
| `--all` | Restart every running proxy (incompatible with `--config`) |

#### `mcpr proxy reload <name> -c <path>`

Hot-reload a running proxy's config **without dropping sessions**. Refreshes the snapshot from `<path>` then sends SIGHUP to the proxy, which atomically swaps live-reloadable settings.

`--config` is **required** — reload always applies a specific file so the source of truth is explicit. To restart instead, see `mcpr proxy restart`.

```bash
mcpr proxy reload localhost-9000 -c mcpr.toml
```

Live-reloadable fields: `[csp]` rules, including widget-scoped overrides.

Everything else (`mcp` upstream, `port`, `widgets`, `tunnel.*`, timeouts, body limits, `[cloud]`, `[runtime]`) requires a full `mcpr proxy restart`. If the new config changes any of those, the reload is **rejected** and the proxy keeps running on the old config — the rejection names the field(s) in `mcpr proxy logs <name>`.

| Flag | Description |
|------|-------------|
| `-c, --config PATH` | **Required.** Config file to snapshot and apply |

### Setup

#### `mcpr proxy setup`

Interactive wizard that connects your proxy to mcpr Cloud. Authenticates via email, lets you pick or create a project and server, generates a project token, and writes `mcpr.toml`.

```bash
mcpr proxy setup                         # writes ./mcpr.toml
mcpr proxy setup -o /path/to/mcpr.toml   # write to a specific path
```

The wizard:
1. Sends a login code to your email
2. Authenticates with mcpr Cloud
3. Lets you select or create a project
4. Lets you select or create a server
5. Optionally creates a tunnel endpoint
6. Generates a project token
7. Writes `mcpr.toml` with `[cloud]` config filled in

If `mcpr.toml` already exists, the wizard reads existing values as defaults.

### Relay Lifecycle

The relay is a singleton tunnel server. One relay per machine.

| Command | Description |
|---------|-------------|
| `mcpr relay run <config>` | Run relay in foreground (no daemon required) |
| `mcpr relay start <config>` | Start relay in background (requires daemon) |
| `mcpr relay stop` | Stop the relay |
| `mcpr relay restart` | Stop + start from saved config snapshot |
| `mcpr relay restart <config>` | Stop + start with new config |
| `mcpr relay status` | Show relay PID, port, uptime |

`mcpr relay run` runs in the foreground and does not require a running daemon. Use this for Docker, systemd, and debugging.

`mcpr relay start` forks to the background and requires a running daemon. The relay participates in daemon lifecycle: `mcpr stop` stops it, `mcpr restart` re-launches it, and it self-terminates if the daemon dies.

```bash
mcpr start                    # start daemon
mcpr relay start relay.toml   # start relay in background
mcpr relay status             # check status
mcpr relay stop               # stop relay
```

Relay config does not need `mode = "relay"` when using `mcpr relay` commands.

### Query & Observe

All `mcpr proxy` commands read the local SQLite store directly — they work even when the daemon isn't running. Pass the proxy name as a positional argument to scope to a specific proxy, or omit it to show data across all proxies.

#### `mcpr proxy logs [name]`

Show recent MCP request logs.

```bash
mcpr proxy logs                          # auto-detect proxy name
mcpr proxy logs --follow                 # tail -f mode (poll every 500ms)
mcpr proxy logs --tool search_products   # filter by tool
mcpr proxy logs --method tools/call      # filter by MCP method
mcpr proxy logs --session 6294           # filter by session (prefix match)
mcpr proxy logs --status error           # filter by status
mcpr proxy logs --since 30m              # last 30 minutes
mcpr proxy logs --tail 100               # last 100 rows
mcpr proxy logs --json                   # NDJSON output
```

| Flag | Default | Description |
|------|---------|-------------|
| `-f, --follow` | false | Poll for new rows every 500ms |
| `--tail N` | 50 | Number of recent rows to show |
| `--since DURATION` | 1h | Time window (e.g., `30m`, `2h`, `7d`). Omitted when `--session` is used (shows all time). |
| `--tool NAME` | — | Filter to a specific tool |
| `--method METHOD` | — | Filter by MCP method (e.g., `tools/call`, `resources/read`, `initialize`) |
| `--session ID` | — | Filter by session ID (supports prefix matching, e.g. first 8 chars) |
| `--status STATUS` | — | Filter by `ok`, `error`, or `timeout` |
| `--json` | false | Output as newline-delimited JSON |

#### `mcpr proxy slow [name]`

Show the slowest requests above a latency threshold.

```bash
mcpr proxy slow                          # default: 500ms threshold
mcpr proxy slow --threshold 1s           # only calls > 1 second
mcpr proxy slow --tool search_products   # slow calls for a specific tool
mcpr proxy slow --since 24h --limit 50   # last 24h, top 50
mcpr proxy slow --json
```

| Flag | Default | Description |
|------|---------|-------------|
| `--threshold DURATION` | 500ms | Minimum latency to include |
| `--since DURATION` | 1h | Time window |
| `--tool NAME` | — | Filter to a specific tool |
| `--limit N` | 20 | Maximum rows |
| `--json` | false | NDJSON output |

#### `mcpr proxy sessions [name]`

List MCP sessions with client info.

```bash
mcpr proxy sessions                      # all sessions in last hour
mcpr proxy sessions --active             # only active (seen in last 5min)
mcpr proxy sessions --client claude-desktop
mcpr proxy sessions --json
```

| Flag | Default | Description |
|------|---------|-------------|
| `--active` | false | Only sessions active in last 5 minutes |
| `--client NAME` | — | Filter by client name |
| `--since DURATION` | 1h | Session start window |
| `--limit N` | 50 | Maximum rows |
| `--json` | false | NDJSON output |

#### `mcpr proxy status [name]`

Show a proxy status overview: request count, error rate, active sessions, and per-tool breakdown.

```bash
mcpr proxy status                        # auto-detect proxy name
mcpr proxy status --since 24h            # last 24 hours
mcpr proxy status --json
```

Output:
```
STATUS — localhost-9000 — last 1h

  Total requests:    1,847
  Error rate:        1.2%
  Sessions:          5 total   2 active

  TOOL                        CALLS        AVG        P95        MAX     ERR%
  search_products               847       142ms      312ms      891ms     0.2%
  create_order                  289       341ms    1,200ms    4,201ms     6.2%

  ACTIVE SESSIONS:
    a1b2c3d4 — claude-desktop 1.2.0 — 42 calls
    e5f6a7b8 — cursor 0.44.1 — 8 calls
```

| Flag | Default | Description |
|------|---------|-------------|
| `--since DURATION` | 1h | Activity window |
| `--json` | false | JSON output |

#### `mcpr proxy session <session_id>`

Drill into a single session — show session metadata and all its request logs. Supports prefix matching (like git commit hashes).

```bash
mcpr proxy session a1b2c3d4                               # prefix match
mcpr proxy session a1b2c3d4-e5f6-7890-abcd-1234567890ab   # full ID
mcpr proxy session a1b2c3d4 --json
```

Output:
```
SESSION — a1b2c3d4-e5f6-7890-abcd-1234567890ab

  Client:      claude-desktop 1.2.0 (claude)
  Status:      active
  Started:     2026-04-10 16:14:04
  Last seen:   2026-04-10 16:28:33
  Calls: 42   Errors: 1

  TIME                 METHOD           TOOL               LATENCY   STATUS
  2026-04-10 16:14:04  initialize       —                    23ms       ok
  2026-04-10 16:14:05  tools/list       —                    12ms       ok
  2026-04-10 16:14:10  tools/call       search_products     142ms       ok
  2026-04-10 16:14:15  tools/call       create_order        891ms    error
```

| Flag | Default | Description |
|------|---------|-------------|
| `--json` | false | JSON output (includes full session + all requests) |

#### `mcpr proxy clients [name]`

Show AI client breakdown across sessions.

```bash
mcpr proxy clients                       # last 7 days
mcpr proxy clients --since 30d
mcpr proxy clients --json
```

Output:
```
CLIENTS — localhost-9000 — last 7d

  CLIENT              VERSION    PLATFORM   SESSIONS    CALLS   ERRORS
  claude-desktop      1.2.0      claude           12    4,201        8
  cursor              0.44.1     cursor            3      891        0
```

| Flag | Default | Description |
|------|---------|-------------|
| `--since DURATION` | 7d | Lookback window |
| `--json` | false | NDJSON output |

#### `mcpr proxy schema [name]`

Show the captured MCP server schema — tools, resources, prompts, and server capabilities. The proxy feeds every schema discovery response (`initialize`, `tools/list`, `resources/list`, `prompts/list`, `resources/templates/list`) into a `SchemaManager` that merges paginated responses, hashes the result, and only writes a new row when the content changes.

```bash
mcpr proxy schema                        # all proxies, current snapshot
mcpr proxy schema my-api                 # filter to one proxy by name
mcpr proxy schema my-api --changes       # change history for one proxy
mcpr proxy schema --unused               # tools listed but never called
mcpr proxy schema --unused --since 30d   # usage window (default: 7d)
mcpr proxy schema --method tools/list    # filter to a specific method
mcpr proxy schema --json                 # JSON output
mcpr proxy schema --changes --limit 100  # last 100 changes
```

Default output:
```
Server: my-mcp-server v1.2.0 (MCP 2025-03-26)
Capabilities: tools, resources
Schema: complete
Last captured: 2026-04-12 14:30:00

── tools/list ──  (captured 2026-04-12 14:30:00)
  Tools (3):
    search_products  —  Search the product catalog by keyword
    get_product      —  Get product details by ID
    create_order     —  Create a new order
```

Change history (`--changes`):
```
  TIME                  METHOD                       CHANGE                 ITEM
  2026-04-12 14:30:00   tools/list                   tool_added             send_email
  2026-04-10 09:15:00   tools/list                   tool_modified          search_products
  2026-04-08 11:00:00   tools/list                   initial                —
```

Schema status is computed from captured data:

| Status | Meaning |
|--------|---------|
| `unknown` | No schema captured yet |
| `partial` | Some discovery methods captured, but not all |
| `complete` | `initialize` + at least one list method captured |

Unused tools (`--unused`):
```
TOOL USAGE — localhost-9000 — last 7d   2/5 unused

  TOOL                             CALLS   ERRORS          LAST CALLED  STATUS
  send_email                           0        0                never  unused
  internal_debug                       0        0                never  unused
  search_products                    847        3    2026-04-12 14:30  ok
  get_product                        312        0    2026-04-12 14:15  ok
  create_order                        89        8    2026-04-12 13:00  errors

  2 tools listed but never called in the last 7d.
```

| Flag | Default | Description |
|------|---------|-------------|
| `--changes` | false | Show change history instead of current schema |
| `--unused` | false | Show tool usage — listed vs actually called |
| `--since DURATION` | 7d | Usage lookback window (with `--unused`) |
| `--method METHOD` | — | Filter to a specific MCP method |
| `--limit N` | 50 | Number of change history rows (with `--changes`) |
| `--json` | false | JSON output |

### Storage

mcpr stores all request data in a local SQLite database at `~/.mcpr/store.db`.

#### `mcpr store stats`

Show database size, row counts, and record age.

```bash
mcpr store stats
```

Output:
```
STORAGE — /Users/you/Library/Application Support/mcpr/mcpr.db

  Total requests:    1,284,847
  Total sessions:    4,201
  Proxies tracked:   1
  Oldest record:     2026-03-01 08:12:44
  Newest record:     2026-04-06 10:18:33

  Database file:     847.0 MB
  WAL file:          2.1 MB
```

#### `mcpr store vacuum --before DURATION`

Delete old records and reclaim disk space.

```bash
mcpr store vacuum --before 7d            # delete records older than 7 days
mcpr store vacuum --before 30d --dry-run # preview what would be deleted
mcpr store vacuum --before 7d --proxy localhost-9000  # scope to one proxy
```

| Flag | Description |
|------|-------------|
| `--before DURATION` | Delete records older than this (required) |
| `--proxy NAME` | Scope to one proxy |
| `--dry-run` | Preview without deleting |

### Config & Info

| Command | Description |
|---------|-------------|
| `mcpr update` | Update mcpr to the latest version |
| `mcpr validate` | Validate `mcpr.toml` and exit |
| `mcpr validate -c path/to/mcpr.toml` | Validate a specific file |
| `mcpr version` | Print version as JSON |

## Duration Format

All `--since`, `--before`, and `--threshold` flags accept human-friendly durations:

| Suffix | Meaning | Example |
|--------|---------|---------|
| `s` | seconds | `30s` |
| `m` | minutes | `15m` |
| `h` | hours | `2h` |
| `d` | days | `7d` |
| `w` | weeks | `2w` |
| `ms` | milliseconds (threshold only) | `500ms` |

## Proxy Name

The proxy name is used in all `mcpr proxy` commands to identify which proxy's data to query. It is derived from (in order):

1. `name = "my-proxy"` in `mcpr.toml`
2. The config filename stem (e.g., `search.toml` → `search`)
3. `"default"` if the config is `mcpr.toml` or unspecified

Run `mcpr status` or `mcpr proxy list` to see proxy names.

## Files

All state lives under `~/.mcpr/`. See [ARCHITECTURE.md](ARCHITECTURE.md) for full details.

| File | Purpose |
|------|---------|
| `~/.mcpr/mcprd.pid` | Daemon process tracking |
| `~/.mcpr/mcprd.log` | Daemon stdout/stderr |
| `~/.mcpr/store.db` | Request storage (SQLite) |
| `~/.mcpr/proxies/{name}/config.toml` | Config snapshot (immutable after creation) |
| `~/.mcpr/proxies/{name}/lock` | Proxy PID, port, timestamp, daemon PID |
| `~/.mcpr/proxies/{name}/proxy.log` | Proxy stdout/stderr |
| `~/.mcpr/relay/config.toml` | Relay config snapshot |
| `~/.mcpr/relay/lock` | Relay PID, port, timestamp |
| `~/.mcpr/relay/relay.log` | Relay stdout/stderr |
