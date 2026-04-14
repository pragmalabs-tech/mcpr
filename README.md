# mcpr

[![CI](https://github.com/cptrodgers/mcpr/actions/workflows/check.yml/badge.svg)](https://github.com/cptrodgers/mcpr/actions/workflows/check.yml)
[![codecov](https://codecov.io/gh/cptrodgers/mcpr/branch/main/graph/badge.svg)](https://codecov.io/gh/cptrodgers/mcpr)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue)](LICENSE)

**A reverse proxy for MCP servers.** Works like nginx or Kong, but at the MCP protocol level — it parses JSON-RPC messages to route, observe, authenticate, and secure MCP traffic. Written in Rust, with under 0.3ms overhead.

```
AI Client (ChatGPT, Claude, Cursor)
        │
        ▼
      mcpr       ← routes, observes, authenticates, secures
        │
        ▼
  Your MCP Server
```

![mcpr TUI dashboard showing information](docs/mcpr-demo.gif)

### Quickstart

```bash
curl -fsSL https://mcpr.app/install.sh | sh
```

```toml
# mcpr.toml
mcp = "http://localhost:9000"
port = 3000
```

```bash
mcpr proxy run
```

---

## Why it exists

General-purpose proxies like nginx and Kong operate at the HTTP level. mcpr operates at the MCP level — it parses JSON-RPC message bodies and extracts MCP-specific fields (method name, tool name, session ID, response status) from each request.

This enables per-tool metrics, schema change tracking, widget CSP rewriting, and MCP-spec OAuth — built in, not through plugins or custom configuration.

---

## Route

mcpr inspects each JSON-RPC body to classify the MCP method. Requests route to the MCP backend. Non-JSON-RPC requests (HTML, JS, CSS, images) route to the widget server if configured.

```toml
mcp = "http://localhost:9000"
widgets = "http://localhost:4444" # For MCP Apps
```

| Category | Methods |
|----------|---------|
| **Lifecycle** | `initialize`, `notifications/initialized`, `ping` |
| **Tools** | `tools/list`, `tools/call`, `notifications/tools/list_changed` |
| **Resources** | `resources/list`, `resources/read`, `resources/subscribe`, `resources/unsubscribe` |
| **Prompts** | `prompts/list`, `prompts/get` |
| **Utility** | `logging/setLevel`, `completion/complete`, `notifications/cancelled`, `notifications/progress` |

Unrecognized methods pass through unchanged.

### Widget CSP

MCP Apps (ChatGPT Apps, Claude connectors) render widgets in sandboxed iframes with CSP rules. ChatGPT reads `openai/widgetCSP`, Claude reads `ui.csp`. Each platform interprets domain lists differently.

mcpr rewrites CSP domain arrays in `tools/list`, `resources/list`, and `resources/read` JSON-RPC responses. It replaces localhost URLs with the proxy's public domain, adds configured extra domains, and adapts the CSP format to match the connecting platform.

```toml
[csp]
mode = "extend"                         # "extend" (default) or "override"
domains = ["api.stripe.com", "cdn.example.com"]
```

---

## Observe

mcpr records every MCP request to a local SQLite database — tool name, latency, status, error code, request/response size, session ID. All `mcpr proxy` commands read from this store and work whether or not the daemon is running.

### Per-tool metrics

```bash
$ mcpr proxy stats
STATS — localhost-9000 — last 1h   Total: 1,284 calls   Errors: 2.3%

  TOOL                  CALLS      AVG      P95      MAX   ERRORS   BYTES IN  BYTES OUT
  get_weather             412    45ms    120ms    340ms      0%     48.2 KB    196.8 KB
  search_docs             389    82ms    210ms    890ms     1.5%    92.1 KB    1.2 MB
  run_query               156   240ms    890ms   2.40s      8.3%   128.4 KB    2.8 MB
```

### Request logs

```bash
mcpr proxy logs --tool search_docs --status error    # failed calls to search_docs
mcpr proxy logs --follow                             # live tail (polls every 500ms)
mcpr proxy logs --session abc123                     # filter by session (prefix match)
mcpr proxy logs --method tools/call                  # filter by MCP method
mcpr proxy logs --since 30m --tail 100               # last 30 minutes, 100 rows
```

### Slow calls

```bash
$ mcpr proxy slow --threshold 500ms
  TOOL              LATENCY    TIME                   STATUS
  run_query          2.40s     2025-04-10 14:20:12    ok
  run_query          1.80s     2025-04-10 14:18:45    ok
  search_docs         890ms    2025-04-10 14:15:33    error
```

### Schema capture

mcpr intercepts `tools/list`, `resources/list`, and `prompts/list` responses as they pass through. It stores the server's schema and records each change.

```bash
$ mcpr proxy schema
Server: my-mcp-server v1.2.0 (MCP 2025-03-26)
Capabilities: tools, resources
Schema: complete
Last captured: 2026-04-12 14:30:00

── tools/list ──
  Tools (3):
    search_products  —  Search the product catalog by keyword
    get_product      —  Get product details by ID
    create_order     —  Create a new order
```

```bash
$ mcpr proxy schema --changes
  TIME                  METHOD        CHANGE           ITEM
  2026-04-12 14:30:00   tools/list    tool_added       send_email
  2026-04-10 09:15:00   tools/list    tool_modified    search_products
```

`mcpr proxy schema --unused` compares listed tools against actual call logs to find tools that are registered but never called.

### Sessions and clients

```bash
$ mcpr proxy sessions
  SESSION    CLIENT                 STARTED         CALLS   ERRS
  a1b2c3d4   claude-desktop 1.2.0   Apr 10 14:20      45      2
  e5f6g7h8   cursor 0.48.0          Apr 10 14:15      23      0

$ mcpr proxy clients
  CLIENT              VERSION    PLATFORM   SESSIONS    CALLS   ERRORS
  claude-desktop      1.2.0      claude           12    4,201        8
  cursor              0.44.1     cursor            3      891        0
```

---

## Authenticate

*Coming soon.*

mcpr will handle MCP OAuth 2.1 and API key auth at the proxy layer. The MCP server receives a verified `x-user-id` header instead of implementing its own auth flow.

---

## Secure

*Coming soon.*

mcpr will provide request validation, per-tool ACLs, and IP whitelisting at the proxy layer.

---

## Comparison

| | mcpr | FastMCP | mcp-proxy | Kong / Envoy |
|---|---|---|---|---|
| **What it is** | Reverse proxy (sits before server) | Framework for building MCP servers | Forward proxy (stdio↔HTTP bridge) | HTTP API gateway |
| **Parses MCP JSON-RPC** | Yes | Yes (inside the server) | Transport conversion only | No |
| **Per-tool metrics** | Built-in (SQLite + CLI) | No | No | No (sees HTTP, not tools) |
| **Schema tracking** | Built-in | No | No | No |
| **CSP rewriting** | Built-in | No | No | No |
| **Auth** | OAuth 2.1 at proxy *(coming soon)* | Built-in OAuth (Python/TS servers) | No | HTTP-level auth |
| **Language** | Rust (single binary) | Python / TypeScript | Python / TypeScript | Go / C++ |

FastMCP is for building MCP servers. mcp-proxy bridges transports on the client side. Kong and Envoy are HTTP gateways. mcpr is a reverse proxy that operates at the MCP protocol level.

---

## Configuration (`mcpr.toml`)

`mcpr.toml` declares proxy behavior. The CLI manages the daemon process. See [docs/CONFIGURATION.md](docs/CONFIGURATION.md) for the full reference.

```toml
# Minimal
mcp = "http://localhost:9000"
port = 3000
```

```toml
# Full
mcp = "http://localhost:9000"
widgets = "http://localhost:4444"
port = 3000

[tunnel]
enabled = true
relay_url = "https://tunnel.mcpr.app"
token = "your-token-here"
subdomain = "myapp"                     # → https://myapp.tunnel.mcpr.app

[csp]
mode = "extend"
domains = ["api.stripe.com", "cdn.example.com"]

[cloud]
token = "mcpr_xxxxxxxx"
server = "my-server"

[logging]
file = true
dir = "./logs"
rotation = "daily"

[store]
# enabled = true                        # default
# name = "api-server"                   # default: derived from mcp URL

# Resource limits
max_request_body_size = 5242880         # 5 MB (default)
max_response_body_size = 10485760       # 10 MB (default)
max_concurrent_upstream = 100           # default
connect_timeout = 5                     # seconds (default)
request_timeout = 30                    # seconds (default)
```

---

## CLI

The CLI manages the daemon, proxies, and relay, and queries the local SQLite store. See [docs/CLI.md](docs/CLI.md) for the full reference.

### Daemon

```
mcpr start                     Start daemon (background)
mcpr start --foreground        Start in foreground (Docker/systemd)
mcpr stop                      Stop proxies + relay + daemon
mcpr restart                   Stop + start, re-launch proxies and relay
mcpr status                    PID, port, uptime, proxy list
```

### Proxy

```
mcpr proxy run [config]        Run a proxy from a config file (--replace)
mcpr proxy start <name>        Start a stopped proxy from saved config
mcpr proxy stop [name]         Stop a proxy (--all)
mcpr proxy restart [name]      Restart a proxy from saved config (--all)
mcpr proxy list                List all proxies and their status (--json)
```

### Relay

```
mcpr relay run [config]        Run relay in foreground (no daemon required)
mcpr relay start [config]      Start relay in background (requires daemon)
mcpr relay stop                Stop the relay
mcpr relay restart [config]    Restart relay (uses saved config if omitted)
mcpr relay status              Show relay PID, port, uptime
```

### Observe

```
mcpr proxy stats [name]        Per-tool metrics (--since, --json)
mcpr proxy logs [name]         Request log (--follow, --tool, --method, --session, --status, --error_code, --since, --tail, --json)
mcpr proxy slow [name]         Slow calls (--threshold, --tool, --follow, --since, --limit, --json)
mcpr proxy status [name]       Overview: requests, error rate, active sessions (--since, --json)
mcpr proxy sessions [name]     Sessions (--active, --client, --since, --limit, --json)
mcpr proxy session <id>        Single session detail (prefix match) (--json)
mcpr proxy clients [name]      Client breakdown (--since, --json)
mcpr proxy schema [name]       Server schema (--changes, --unused, --method, --since, --limit, --json)
```

`[name]` is optional when one proxy is running — auto-detected from the daemon.

### Storage

```
mcpr store stats               Database size and row counts
mcpr store vacuum --before 7d  Delete old records (--proxy, --dry-run)
```

### Utility

```
mcpr update                    Update to latest version
mcpr validate                  Validate mcpr.toml (--dump to print resolved config)
mcpr version                   Print version (JSON)
```

All query commands support `--json` for piping into `jq` or scripts.

---

## Roadmap

**Routing**
- [x] JSON-RPC routing
- [ ] Multi-server routing (one mcpr URL, many MCP backends)

**Observability**
- [x] MCP request logs, session tracking, AI client tracking
- [x] MCP schema capture with change tracking
- [x] Per-tool metrics (calls, error%, p50, p95, max, bytes)
- [x] Widget CSP rewriting (auto-detection, per-platform adaptation)
- [x] Cloud dashboard sync ([mcpr.app](https://cloud.mcpr.app))

**Auth**
- [ ] Token API auth at the proxy
- [ ] OAuth 2.1 at the proxy (legacy auth, OAuth providers, or in-house)

**Security**
- [ ] Per-tool access control
- [ ] Rate limiting and circuit breaker
- [ ] IP whitelist

**Tunnel/Relay**
- [x] Built-in tunnel client and self-hosted relay server
- [x] Standalone `mcpr relay` CLI with daemon lifecycle
- [x] Daemon mode, graceful shutdown

## License

Apache 2.0
