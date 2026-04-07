# mcpr

**The proxy for MCP servers.**
Route, log, and secure MCP traffic — from dev to production.

```bash
mcpr run --mcp http://localhost:9000 --port 3000
```

![mcpr TUI dashboard showing proxied MCP requests with tool names and latency](docs/mcpr-demo.gif)

---

## What It Does

mcpr sits between AI clients (ChatGPT, Claude, VS Code, Cursor) and your MCP server. It parses every JSON-RPC message at the protocol level — not as raw HTTP — so it can route, observe, and secure MCP traffic in ways generic proxies can't.

- **Route** — MCP-aware reverse proxy. Tool calls, resource reads, session handshakes — all parsed and forwarded correctly.
- **Observe** — Structured events for every request: tool name, latency, status, session ID. Pipe to stdout, or [mcpr.app](https://mcpr.app).
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
| **Local dev** | `mcpr run --mcp :9000` |
| **Dev + tunnel** | `mcpr run --mcp :9000` (auto) |
| **VPS / VM** | `mcpr run --mcp :9000 --no-tunnel` |
| **Docker** | `docker run -p 3000:3000 -p 9901:9901 -v ./mcpr.toml:/app/mcpr.toml ghcr.io/cptrodgers/mcpr:latest` |
| **Kubernetes** | Helm chart (coming soon) |

> **Backward compat:** `mcpr --mcp ...` (no subcommand) still works and defaults to `run`.

## Observability

Every MCP request emits a structured JSON event:

```bash
mcpr run --mcp :9000 --events
```

```json
{
  "ts": "2026-04-06T10:15:33Z",
  "type": "tool_call",
  "method": "tools/call",
  "tool": "search_products",
  "latency_ms": 142,
  "status": "ok",
  "session": "sess_abc123"
}
```

Pipe to any log aggregator — or add one line to stream to [mcpr.app](https://mcpr.app) for a full dashboard:

```toml
[cloud]
token = "mcpr_xxxxxxxx"
server = "my-server"          # matches server name in your mcpr.app project
# endpoint = "https://api.mcpr.app"  # optional, default
# batch_size = 100                    # optional, events per batch
# flush_interval_ms = 5000           # optional, flush interval
```

### mcpr.app Dashboard

The cloud dashboard gives you:

- **Tool Health** — per-tool status (healthy/degraded/down), error rate, p50/p95/p99 latency, sortable table
- **Client Breakdown** — traffic by AI client (ChatGPT, Claude, VS Code, Cursor), detected from MCP `initialize` handshake
- **Sessions** — per-session timeline with expandable event detail, active/ended status
- **Latency Charts** — overall percentile timeline + per-tool line chart with p50/p95/p99 toggle
- **Slow Calls** — outlier detection with "vs p50" multiplier, request/response sizes
- **Error Grouping** — errors grouped by tool + message with timeline, first/last seen
- **Event Log** — live streaming log with search, filters, export
- **Global Filters** — filter by tools (multi-select), client, status, and custom time ranges

<!-- SCREENSHOT: replace with actual dashboard screenshot -->
![mcpr.app dashboard](docs/cloud-dashboard.png)

Answer questions like:
- "Which tool is slow?" — sort by p95, see the latency bar
- "Is it my server or the client?" — filter by ChatGPT vs Claude vs VS Code
- "What happened in this user's session?" — click a session ID, see every event in order
- "When did errors start?" — error timeline shows exactly when and which tools broke

## CSP Handling

MCP Apps (ChatGPT Apps, Claude connectors) render widgets in sandboxed iframes with strict CSP. Every platform enforces it differently. mcpr handles this automatically:

- Rewrites CSP domain arrays in JSON-RPC response metadata (`tools/list`, `tools/call`, `resources/list`, `resources/read`)
- Replaces localhost/upstream URLs with the proxy (tunnel) domain
- Adds extra domains from config to `connectDomains` and `resourceDomains`
- Adapts format per platform (ChatGPT uses `openai/widgetCSP`, Claude uses `ui.csp`)
- Deep-scans the entire response to catch CSP arrays in nested structures
- Supports two modes: **extend** (keep external domains, strip localhost) or **override** (only configured domains)

Zero config in extend mode. Works on first proxy.

## Health & Admin

mcpr runs an admin API on port `9901` (configurable via `--admin-bind`):

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

## CLI Subcommands

```
mcpr run [OPTIONS]       Start proxy in foreground (default)
mcpr validate [-c PATH]  Validate config and exit
mcpr version             Print version info as JSON
```

## Getting Started

### Proxy an MCP server

```bash
mcpr run --mcp http://localhost:9000
```

### Proxy MCP server + widget dev server

```toml
# mcpr.toml
mcp = "http://localhost:9000"
widgets = "http://localhost:4444"
```

```bash
mcpr
```

### Serve static widgets from disk

```toml
# mcpr.toml
mcp = "http://localhost:9000"
widgets = "./widgets"
```

### Add extra CSP domains

```toml
# mcpr.toml
mcp = "http://localhost:9000"

[csp]
domains = ["api.stripe.com", "cdn.example.com"]
```

## Roadmap

- [x] MCP proxy (route JSON-RPC to upstream)
- [x] Widget proxy (merge MCP + widget assets)
- [x] CSP auto-detection and enrichment
- [x] Platform adaptation (ChatGPT / Claude / VS Code)
- [x] Edge config (CSP, domains, OAuth URLs)
- [x] Structured events (`--events`)
- [x] TUI dashboard
- [x] Cloud dashboard ([mcpr.app](https://mcpr.app))
- [x] Cloud sync
- [x] Per-tool health (calls, errors, p50/p95)
- [x] Subcommand CLI (`run`, `validate`, `version`)
- [x] Admin API with health endpoints (`/healthz`, `/ready`)
- [x] SIGTERM graceful drain for Kubernetes
- [x] Structured stderr logging (JSON/pretty)
- [x] `--tui` / `--no-tui` flags for Docker/CI
- [ ] Prometheus metrics (`/metrics`)
- [ ] SIGHUP config reload
- [ ] OAuth 2.1 at the proxy
- [ ] ACL (per-tool access control)
- [ ] Multi-server routing
- [ ] Rate limiting + circuit breaker
- [ ] Widget injection (add widgets to tool-only servers)
- [ ] OTLP ingestion

## Also Includes

A built-in tunnel for development — public HTTPS URL with zero setup, no ngrok needed. And [mcpr.app](https://mcpr.app) Studio for testing MCP servers in the browser.

## License

Apache 2.0
