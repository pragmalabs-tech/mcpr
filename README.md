# mcpr

[![CI](https://github.com/cptrodgers/mcpr/actions/workflows/check.yml/badge.svg)](https://github.com/cptrodgers/mcpr/actions/workflows/check.yml)
[![codecov](https://codecov.io/gh/cptrodgers/mcpr/branch/main/graph/badge.svg)](https://codecov.io/gh/cptrodgers/mcpr)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue)](LICENSE)

**The proxy for MCP servers.**
Route, observe, and secure MCP traffic — from dev to production.

```bash
mcpr start
```

![mcpr TUI dashboard showing information](docs/mcpr-demo.gif)

---

## What It Does

mcpr sits between AI clients (ChatGPT, Claude, VS Code, Cursor) and your MCP server. It parses every JSON-RPC message at the protocol level — not as raw HTTP — so it can route, observe, and secure MCP traffic in ways generic proxies can't.

- **Route** — MCP-aware reverse proxy. Tool calls, resource reads, session handshakes — all parsed and forwarded correctly.
- **Observe** — Structured events for every request: tool name, latency, status, error codes, bytes, session ID. Query locally via CLI.
- **Per-tool health** — Call count, error rate, p95/max latency, bytes in/out — per tool, at a glance.
- **Handle CSP** — Rewrites CSP domain arrays in JSON-RPC metadata per platform (ChatGPT and Claude). Zero config.
- **Edge config** — Change CSP, OAuth URLs, and domain settings at the proxy. No server redeploy.

## Install

```bash
curl -fsSL https://mcpr.app/install.sh | sh
```

## Deploy Anywhere

Single Rust binary. No JVM, no Kubernetes, no database.

| Environment | How |
|---|---|
| **Local dev** | `mcpr start` |
| **Dev + tunnel** | `mcpr start` (with `[tunnel]` in `mcpr.toml`) |
| **VPS / VM** | `mcpr start` |
| **Docker** | `docker run -p 3000:3000 -p 9901:9901 -v ./mcpr.toml:/app/mcpr.toml ghcr.io/cptrodgers/mcpr:latest` |
| **Kubernetes** | Helm chart (coming soon) |

> `mcpr start` runs as a background daemon. Use `mcpr start --foreground` for Docker/systemd.

## Observability

Every MCP request is recorded to a local SQLite store — tool name, latency, status, error codes, request/response sizes, session ID. Query anytime, no running daemon needed:

### Per-Tool Health Dashboard

```bash
$ mcpr proxy stats
STATS — localhost-9000 — last 1h   Total: 1,284 calls   Errors: 2.3%

  TOOL                    CALLS      AVG      P95      MAX   ERRORS   BYTES IN  BYTES OUT  AVG SIZE
  get_weather               412    45ms    120ms    340ms      0%     48.2 KB    196.8 KB      610
  search_docs               389    82ms    210ms    890ms     1.5%    92.1 KB    1.2 MB      3.4 KB
  run_query                 156   240ms    890ms   2.40s      8.3%   128.4 KB    2.8 MB     19.2 KB
  <initialize>              142    12ms     28ms     65ms      0%      8.1 KB     14.2 KB      157
  <resources/read>           98    35ms     95ms    180ms      0%     12.0 KB    456.0 KB    4.8 KB
  <prompts/get>              87    18ms     42ms     78ms      0%      4.8 KB     28.4 KB      381
```

Every tool gets its own row: call count, avg/p95/max latency, error rate, and bytes at a glance.

### Request Logs

```bash
$ mcpr proxy logs
TIME                  METHOD                       TOOL                             LATENCY       IN      OUT       ERR  STATUS
2025-04-10 14:23:01   tools/call                   get_weather                        45ms    120 B    492 B            ok
2025-04-10 14:23:00   tools/call                   run_query                         1.20s    2.1 KB   48 KB            ok
2025-04-10 14:22:58   tools/call                   search_docs                       210ms    450 B    8.2 KB  -32601   error  "Method not found"
2025-04-10 14:22:55   resources/read               —                                  35ms     64 B    1.2 KB           ok
2025-04-10 14:22:50   initialize                   —                                  12ms    180 B    320 B            ok
```

Filter and live-tail:

```bash
mcpr proxy logs --follow                     # live tail (polls every 500ms)
mcpr proxy logs --tool get_weather           # filter by tool name
mcpr proxy logs --method tools/call          # filter by MCP method
mcpr proxy logs --session abc123             # filter by session (prefix match)
mcpr proxy logs --status error               # only errors
mcpr proxy logs --error_code -32601          # filter by JSON-RPC error code
mcpr proxy logs --since 30m --tail 100       # last 30 minutes, 100 rows
mcpr proxy logs --json                       # newline-delimited JSON output
```

### Slow Call Detection

```bash
$ mcpr proxy slow --threshold 500ms
TOP SLOW CALLS — localhost-9000 — last 1h (threshold: 500ms)

  TOOL                             LATENCY       SIZE   TIME                   STATUS
  run_query                         2.40s      51 KB   2025-04-10 14:20:12    ok
  run_query                         1.80s      38 KB   2025-04-10 14:18:45    ok
  search_docs                        890ms     9.1 KB   2025-04-10 14:15:33    error
  run_query                          780ms     22 KB   2025-04-10 14:12:01    ok

  4 calls above threshold in last 1h (avg: 1.47s)
```

```bash
mcpr proxy slow --threshold 1s --tool run_query   # filter by tool
mcpr proxy slow --follow                          # live tail slow calls
```

### Sessions & Clients

```bash
$ mcpr proxy sessions
SESSIONS — localhost-9000 — last 1h

  SESSION    CLIENT                   STARTED           LAST SEEN        CALLS   ERRS
  a1b2c3d4   claude-desktop 1.2.0 ●   Apr 10 14:20      just now           45      2
  e5f6g7h8   cursor 0.48.0 ●          Apr 10 14:15      just now           23      0
  i9j0k1l2   vscode 1.96.0 ○          Apr 10 13:50      Apr 10 14:10       89      5

  3 sessions total   2 active
```

```bash
$ mcpr proxy clients
CLIENTS — localhost-9000 — last 7d

  CLIENT               VERSION    PLATFORM     SESSIONS    CALLS   ERRORS
  claude-desktop       1.2.0      claude              8      312       12
  cursor               0.48.0     cursor              5      198        3
  vscode               1.96.0     vscode              3      145        8

  3 unique clients   16 sessions total
```

```bash
mcpr proxy session a1b2c3d4          # drill into a session (prefix match)
mcpr proxy sessions --active         # only active sessions
mcpr proxy sessions --client cursor  # filter by client
```

### Proxy Status Overview

```bash
$ mcpr proxy status
STATUS — localhost-9000 — last 1h

  Total requests:    1,284
  Error rate:        2.3%
  Sessions:          3 total   2 active

  TOOL                       CALLS        AVG        P95        MAX     ERR%   BYTES IN  BYTES OUT  AVG SIZE
  get_weather                  412      45ms      120ms      340ms     0.0%     48.2 KB    196.8 KB      610
  search_docs                  389      82ms      210ms      890ms     1.5%     92.1 KB    1.2 MB      3.4 KB
  run_query                    156     240ms      890ms     2.40s      8.3%    128.4 KB    2.8 MB     19.2 KB

  ACTIVE SESSIONS:
    a1b2c3d4 — claude-desktop 1.2.0 — 45 calls
    e5f6g7h8 — cursor 0.48.0 — 23 calls
```

## MCP Protocol Support

mcpr parses every JSON-RPC 2.0 message and classifies MCP methods for observability and CSP rewriting.

### Fully Supported Methods

| Category | Methods | Proxy Behavior |
|----------|---------|----------------|
| **Lifecycle** | `initialize`, `notifications/initialized`, `ping` | Extracts client info (name, version, platform). Tracks session state. |
| **Tools** | `tools/list`, `tools/call`, `notifications/tools/list_changed` | **CSP rewriting** on responses. Extracts tool name for per-tool metrics. |
| **Resources** | `resources/list`, `resources/templates/list`, `resources/read`, `resources/subscribe`, `resources/unsubscribe` | **CSP rewriting** on responses. Extracts resource URI for logging. |
| **Prompts** | `prompts/list`, `prompts/get` | Extracts prompt name for logging. |
| **Utility** | `logging/setLevel`, `completion/complete`, `notifications/cancelled`, `notifications/progress` | Extracts request IDs and progress tokens. |

Unknown methods are forwarded as-is — mcpr never blocks traffic. See [docs/SUPPORTED_MCP_METHODS.md](docs/SUPPORTED_MCP_METHODS.md) for the full reference.

## CSP Handling

MCP Apps (ChatGPT Apps, Claude connectors) render widgets in sandboxed iframes with strict CSP. Every platform enforces it differently. mcpr handles this automatically:

- Rewrites CSP domain arrays in JSON-RPC response metadata (`tools/list`, `tools/call`, `resources/list`, `resources/templates/list`, `resources/read`)
- Replaces localhost/upstream URLs with the proxy (tunnel) domain
- Adds extra domains from config to `connectDomains` and `resourceDomains`
- Adapts format per platform (ChatGPT uses `openai/widgetCSP`, Claude uses `ui.csp`)
- Deep-scans the entire response to catch CSP arrays in nested structures
- Supports two modes: **extend** (keep external domains, strip localhost) or **override** (only configured domains)

Zero config in extend mode. Works on first proxy.

## Configuration

All configuration is via `mcpr.toml`. No CLI flags or env vars needed.

```toml
# mcpr.toml — minimal
mcp = "http://localhost:9000"
port = 3000
```

```toml
# mcpr.toml — full
mcp = "http://localhost:9000"       # upstream MCP server (required)
port = 3000                         # proxy port (default: 3000)
widgets = "http://localhost:4444"   # widget source (URL or file path)
drain_timeout = 30                  # graceful shutdown seconds (default: 30)
log_format = "json"                 # "json" (default) or "pretty"
admin_bind = "127.0.0.1:9901"      # admin API address (or "none" to disable)

# Resource limits
max_request_body_size = 5242880     # 5 MB (default)
max_response_body_size = 10485760   # 10 MB (default)
max_concurrent_upstream = 100       # concurrent upstream requests (default)
connect_timeout = 5                 # TCP connect timeout seconds (default)
request_timeout = 30                # total request timeout seconds (default)

[csp]
mode = "extend"                     # "extend" (default) or "override"
domains = ["api.stripe.com", "cdn.example.com"]

[tunnel]
enabled = true                      # public HTTPS URL via relay (no ngrok needed)
relay_url = "https://tunnel.mcpr.app"
subdomain = "myapp"                 # → https://myapp.tunnel.mcpr.app
```

See [docs/CONFIGURATION.md](docs/CONFIGURATION.md) for the full reference including relay mode, cloud sync, and token configuration.

## Health & Admin

mcpr runs an admin API on port `9901` (configurable via `admin_bind`):

| Endpoint | Purpose |
|---|---|
| `GET /healthz` | Liveness — 200 unless shutting down |
| `GET /ready` | Readiness — 503 while draining or MCP upstream disconnected |
| `GET /version` | Version info as JSON |

Kubernetes probes:
```yaml
livenessProbe:
  httpGet: { path: /healthz, port: 9901 }
readinessProbe:
  httpGet: { path: /ready, port: 9901 }
```

## CLI Reference

mcpr runs as a background daemon. Start it, observe it, stop it.

```
mcpr start                     Start proxy daemon (background)
mcpr start --foreground        Start in foreground (Docker/systemd)
mcpr stop                      Stop the daemon
mcpr restart                   Restart the daemon
mcpr status                    Show PID, port, uptime, proxy name
```

### Proxy observability (reads local SQLite — no daemon required)

```
mcpr proxy logs [name]         Request logs
  --follow (-f)                  Live tail (poll every 500ms)
  --tool NAME                    Filter by tool name
  --method METHOD                Filter by MCP method (e.g. tools/call)
  --session ID                   Filter by session ID (prefix match)
  --status STATUS                Filter: ok | error | timeout
  --error_code CODE              Filter by JSON-RPC error code
  --since DURATION               Time window (default: 1h)
  --tail N                       Number of rows (default: 50)
  --json                         Newline-delimited JSON output

mcpr proxy stats [name]        Per-tool health metrics
  --since DURATION               Aggregation window (default: 1h)
  --json                         JSON snapshot

mcpr proxy slow [name]         Slow calls above threshold
  --threshold DURATION           Minimum latency (default: 500ms)
  --tool NAME                    Filter by tool
  --follow (-f)                  Live tail (poll every 1s)
  --since DURATION               Time window (default: 1h)
  --limit N                      Max rows (default: 20)
  --json                         NDJSON output

mcpr proxy status [name]       Proxy overview (requests, errors, active sessions, per-tool breakdown)
  --since DURATION               Activity window (default: 1h)
  --json                         JSON output

mcpr proxy sessions [name]     MCP sessions with client info
  --active                       Only active sessions
  --client NAME                  Filter by client name
  --since DURATION               Time window (default: 1h)
  --limit N                      Max rows (default: 50)
  --json                         NDJSON output

mcpr proxy session <id>        Drill into a single session (prefix match)
  --json                         JSON output

mcpr proxy clients [name]      AI client breakdown
  --since DURATION               Lookback window (default: 7d)
  --json                         NDJSON output
```

### Storage maintenance

```
mcpr store stats               Database size and row counts
mcpr store vacuum              Delete old records, reclaim disk
  --before DURATION              Delete records older than (required, e.g. 7d)
  --proxy NAME                   Scope to one proxy
  --dry-run                      Preview without deleting
```

### Utility

```
mcpr update                    Update mcpr to the latest version
mcpr validate                  Validate mcpr.toml (--dump to print resolved config)
mcpr version                   Print version as JSON
```

`[name]` is optional when one proxy is running — auto-detected from the daemon.

All `--json` flags output machine-readable JSON for piping into `jq`, dashboards, or scripts.

See [docs/CLI.md](docs/CLI.md) for the full reference with examples.

## Getting Started

### 1. Start the proxy

```bash
mcpr start                    # → mcpr daemon started (PID: 12345, port: 3000)
```

### 2. Check status

```bash
mcpr status
# → mcpr daemon is running
#     Proxy: localhost-9000   PID: 12345   Port: 3000
```

### 3. Observe traffic

```bash
mcpr proxy stats              # per-tool health dashboard
mcpr proxy logs --follow      # live request log
mcpr proxy slow --threshold 1s  # find slow calls
mcpr proxy clients            # who's calling?
mcpr proxy sessions --active  # active MCP sessions
```

### 4. Use a config file

```toml
# mcpr.toml
mcp = "http://localhost:9000"
port = 3000
widgets = "http://localhost:4444"

[csp]
domains = ["api.stripe.com", "cdn.example.com"]
```

```bash
mcpr start
```

## Architecture

```
AI Clients                    mcpr                           MCP Server
(ChatGPT, Claude,    ──►  [Route + Observe + CSP]  ──►   (your server)
 VS Code, Cursor)          │
                           ├─► stderr (JSON/pretty logs)
                           └─► SQLite (local query engine)
```

- **Single binary** — no runtime deps, no database to manage
- **Protocol-aware** — parses JSON-RPC 2.0, classifies MCP methods, extracts tool/resource/session metadata
- **Event bus** — every request emits a structured event to all sinks (stderr, SQLite, cloud) in parallel
- **Background daemon** — `mcpr start` forks before tokio, writes PID file, signals readiness
- **Graceful shutdown** — SIGTERM drains in-flight requests (configurable timeout), flushes all sinks

## Roadmap

- [x] MCP-aware reverse proxy (JSON-RPC routing)
- [x] Widget proxy (merge MCP + widget assets)
- [x] CSP auto-detection and enrichment
- [x] Platform adaptation (ChatGPT / Claude / VS Code / Cursor)
- [x] Edge config (CSP, domains, OAuth URLs)
- [x] Per-tool health metrics (calls, error%, p95, max, bytes in/out)
- [x] Daemon mode (`mcpr start/stop/restart/status`)
- [x] SQLite storage engine with CLI query tools
- [x] Full CLI observability (`logs/stats/slow/status/sessions/clients`)
- [x] Live tailing (`--follow` on logs and slow)
- [x] JSON output mode on all commands (`--json`)
- [x] Error code tracking and filtering
- [x] MCP method parsing (tools, resources, templates, prompts, notifications, progress)
- [x] TUI dashboard
- [x] Admin API (`/healthz`, `/ready`, `/version`)
- [x] SIGTERM graceful drain for Kubernetes
- [x] Built-in tunnel (public HTTPS, no ngrok)
- [x] Storage maintenance (`mcpr store stats/vacuum`)
- [ ] `mcpr proxy view` — TUI viewer that attaches to running daemon
- [ ] Multiple proxies in one daemon (`[[proxy]]` config array)
- [ ] Prometheus metrics (`/metrics`)
- [ ] SIGHUP config reload
- [ ] OAuth 2.1 at the proxy
- [ ] ACL (per-tool access control)
- [ ] Multi-server routing
- [ ] Rate limiting + circuit breaker
- [ ] Widget injection (add widgets to tool-only servers)
- [ ] OTLP ingestion

## Also Includes

- **Built-in tunnel** — public HTTPS URL for development, zero setup, no ngrok needed.
- **[mcpr.app](https://mcpr.app)** — advanced features for analytics, debugging, and tunnel management.

## License

Apache 2.0
