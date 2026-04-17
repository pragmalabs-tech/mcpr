# mcpr

[![CI](https://github.com/cptrodgers/mcpr/actions/workflows/check.yml/badge.svg)](https://github.com/cptrodgers/mcpr/actions/workflows/check.yml)
[![codecov](https://codecov.io/gh/cptrodgers/mcpr/branch/main/graph/badge.svg)](https://codecov.io/gh/cptrodgers/mcpr)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue)](LICENSE)

**A proxy for MCP Apps/Server.** Works like nginx or Kong, but at the MCP protocol level — it parses JSON-RPC messages to route, widget, observe, authenticate, and secure MCP traffic. Written in Rust, with under 0.3ms overhead.

Most current MCP proxies focus on the client side, consuming multiple MCP servers from the client's perspective. So, mcpr wants to focus on the server side, at the MCP app/server, supporting routing of JSON-RPC requests together with widget serving, easy CSP configuration, OAuth 2.1 integration, and observing tools and resources performance, not just at the HTTP request level. This way, application code (MCP apps) can focus on the business logic instead of a bunch of messy things needed to deal with AI clients.

It also comes from my personal experience while developing usestudykit.com (a study MCP app), so please give me more feedback on the project.

![mcpr TUI dashboard showing information](docs/mcpr-demo.gif)

### Quickstart

```
AI Client (ChatGPT, Claude, Microsoft Copilot, ...) 
        │
        ▼
  Reserve Proxy (Nginx, Caddy, HaProxy, Kong, ...)
        │
        ▼
      mcpr       ← routes, widgets, observes, authenticates, secures
        │
        ▼
  Your MCP Apps/Server
```

Write a config:

```toml
# mcpr.toml
mcp = "http://localhost:9000"
port = 3000
```

**Run by mcpr** (recommended for local development):

```bash
curl -fsSL https://mcpr.app/install.sh | sh
mcpr proxy run
```

**Run By Docker** (recommended for servers/production):

```bash
docker run -d --name mcpr \
  -v "$(pwd)/mcpr.toml:/etc/mcpr/mcpr.toml:ro" \
  -v mcpr-state:/var/lib/mcpr \
  -p 3000:3000 \
  ghcr.io/cptrodgers/mcpr:latest
```

See [docs/DOCKER.md](docs/DOCKER.md) for volumes, health probes, and compose/Kubernetes examples.


---

## Route

mcpr inspects each JSON-RPC body to classify the MCP method. Requests route to the MCP backend. Non-JSON-RPC requests (HTML, JS, CSS, images) route to the widget server if configured.

```toml
mcp = "http://localhost:9000"
widgets = "http://localhost:4444" # Optional for MCP server (no Apps)
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
$ mcpr proxy status
STATUS — localhost-9000 — last 1h

  Total requests:    1,284
  Error rate:        2.3%
  Sessions:          12 total   3 active

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

mcpr will handle MCP OAuth 2.1 and API key auth at the proxy layer. The MCP Apps (server) receives a verified `x-user-id` header instead of implementing its own auth flow.

---

## Secure

*Coming soon.*

mcpr will provide request validation, per-tool ACLs, and IP whitelisting at the proxy layer.

---

## Configuration (`mcpr.toml`)

`mcpr.toml` declares proxy behavior — upstream MCP URL, port, tunnel, CSP, cloud sync, logging, and resource limits. See [docs/proxy/PROXY_CONFIGURATION.md](docs/proxy/PROXY_CONFIGURATION.md) for the full reference.

```toml
# Minimal
mcp = "http://localhost:9000"
port = 3000
```

---

## CLI

The CLI manages the daemon, proxies, and relay, and queries the local SQLite store. See [docs/CLI.md](docs/CLI.md) for the full command reference.

---

## Roadmap

**Routing & Network**
- [x] JSON-RPC routing 
- [x] Widget CSP rewriting (auto-detection, per-platform adaptation)
- [ ] Widgets mode (Server endpoint, statics)
- [ ] Multi-server routing (one mcpr URL, many MCP backends)

**Observability**
- [x] MCP request logs, session tracking, AI client tracking
- [x] MCP schema capture with change tracking
- [x] Per-tool metrics (calls, error%, p50, p95, max, bytes)
- [x] Cloud dashboard sync ([mcpr.app](https://cloud.mcpr.app))

**Auth**
- [ ] OAuth 2.1 (Auth Provider, Legacy Auth)
- [ ] Multiple Auth Mode for one server
- [ ] Token API auth (Optional because mcp apps support oauth 2.1 only)

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
