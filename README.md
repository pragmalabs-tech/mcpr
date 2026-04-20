# mcpr

[![CI](https://github.com/cptrodgers/mcpr/actions/workflows/check.yml/badge.svg)](https://github.com/cptrodgers/mcpr/actions/workflows/check.yml)
[![codecov](https://codecov.io/gh/cptrodgers/mcpr/branch/main/graph/badge.svg)](https://codecov.io/gh/cptrodgers/mcpr)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue)](LICENSE)

**A proxy for MCP Apps/Servers.** mcpr parses JSON-RPC messages to route, serve widgets, observe, authenticate, and secure MCP traffic. Written in Rust and built for minimal overhead — [under 1ms p99](benches/reports/v0.4.42-post-refactor.md).

![mcpr TUI dashboard showing information](docs/assets/mcpr-demo.gif)

### Why I build mcpr

Most MCP proxies sit on the client side, aggregating multiple servers for a single client. mcpr sits on the server side — in front of your MCP app — handling JSON-RPC routing, widget serving, CSP configuration, OAuth 2.1, and per-tool/per-resource observability. Your application code stays focused on business logic while mcpr absorbs the policy differences between AI clients (ChatGPT, Claude, Copilot, etc.).

Status: Under active development — already running in front of my own MCP App (https://mcp.usestudykit.com/mcp)

### Highlight Features

- **Routing** — forwards `tools/call` and `resources/*` traffic with minimal overhead ([under 1ms p99](benches/reports/v0.4.42-post-refactor.md)), plus CSP rewriting that stays compatible across AI clients (ChatGPT, Claude, etc.).
- **Observability** — per-method stats: tool calls, prompt usage, slow calls, error rates. [Dashboard demo](docs/assets/cloud-dashboard.png) · [proxy demo](docs/assets/proxy-status.png)
- **Sessions capture** — see how each AI client and user interacts with your MCP: client info, call flow, and the full sequence of tool calls within every session.
- **Schema capture** — records the MCP schema as it flows through, tracks changes over time, and flags tools that are registered but never called.
- **Authentication** *(coming soon)* — OAuth 2.1 integration with common providers, plus support for bring-your-own auth that meets the 2.1 spec. Open an issue if your provider isn't covered.

### Quickstart

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

Docker support is in beta — see [docs/DOCKER.md](docs/DOCKER.md) for volumes, health probes, and compose/Kubernetes examples. Feedback welcome.

---

## Route

mcpr inspects each JSON-RPC body to classify the MCP method. Requests route to the MCP backend. Non-JSON-RPC requests (HTML, JS, CSS, images) route to the widget server if configured.

```toml
mcp = "http://localhost:9000"
widgets = "http://localhost:4444" # Optional for MCP server (no Apps)
```

### Widget CSP

MCP Apps (ChatGPT Apps, Claude connectors) render widgets in sandboxed iframes with CSP rules. ChatGPT reads `openai/widgetCSP`, Claude reads `ui.csp`. Each platform interprets domain lists differently.

mcpr rewrites CSP domain arrays in `tools/list`, `tools/call`, `resources/list`, `resources/templates/list`, and `resources/read` responses. It replaces localhost URLs with the proxy's public domain, merges configured domains, and emits the CSP shape each client expects.

CSP has three independent directives — `connectDomains` (fetch / WebSocket), `resourceDomains` (scripts, styles, images), and `frameDomains` (iframes) — each with its own `mode` (`extend` or `replace`). Widget entries layer glob-matched overrides on top of the global policy.

```toml
[csp.connectDomains]
domains = ["api.example.com"]
mode    = "extend"

[csp.resourceDomains]
domains = ["cdn.example.com"]
mode    = "extend"

[csp.frameDomains]
domains = []
mode    = "replace"

[[csp.widget]]
match              = "ui://widget/payment*"
connectDomains     = ["api.stripe.com"]
connectDomainsMode = "extend"
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

## Reference

- Configuration — [docs/proxy/PROXY_CONFIGURATION.md](docs/proxy/PROXY_CONFIGURATION.md) (upstream URL, port, tunnel, CSP, cloud sync, logging, limits)
- CLI — [docs/CLI.md](docs/CLI.md) (daemon, proxies, relay, and SQLite queries)
- Docker — [docs/DOCKER.md](docs/DOCKER.md) (volumes, health probes, compose/Kubernetes)

---

## Roadmap

**Routing & Network**
- [x] JSON-RPC routing 
- [x] Content Security Policy (CSP) rewriting
- [ ] Widgets mode (Server endpoint, statics)

**Observability**
- [x] MCP request logs, session tracking, AI client tracking
- [x] MCP schema capture with change tracking
- [x] Per-tool metrics (calls, error%, p50, p95, max, request size, response size)
- [x] Cloud dashboard sync ([mcpr.app](https://cloud.mcpr.app))

**Auth**
- [ ] OAuth 2.1 for Auth Provider
- [ ] OAuth 2.1 for legacy auth (non oauth standard)
- [ ] Token API auth
- [ ] Multiple Auth Mode for one server

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
