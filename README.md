# mcpr

**The proxy for MCP servers.**
Route, log, and secure MCP traffic — from dev to production.

```bash
mcpr --mcp http://localhost:9000
```

<!-- TODO: screenshot — TUI showing proxied requests with tool names + latency -->

---

## What It Does

mcpr sits between AI clients (ChatGPT, Claude, VS Code, Cursor) and your MCP server. It parses every JSON-RPC message at the protocol level — not as raw HTTP — so it can route, observe, and secure MCP traffic in ways generic proxies can't.

- **Route** — MCP-aware reverse proxy. Tool calls, resource reads, session handshakes — all parsed and forwarded correctly.
- **Observe** — Structured events for every request: tool name, latency, status, session ID. Pipe to stdout, Datadog, or [mcpr.app](https://mcpr.app).
- **Handle CSP** — Scans widget HTML, detects missing domains, injects correct CSP headers per platform. Zero config.
- **Edge config** — Change CSP, OAuth URLs, and domain settings at the proxy. No server redeploy.

## Install

```bash
curl -fsSL https://mcpr.app/install.sh | sh
```

## Deploy Anywhere

| Environment | Command | Replaces |
|---|---|---|
| **Local dev** | `mcpr --mcp :9000` | — |
| **Dev + tunnel** | `mcpr --mcp :9000` (tunnel auto) | ngrok |
| **VPS / VM** | `mcpr --mcp :9000 --no-tunnel` | nginx reverse proxy |
| **Docker** | `docker run mcpr --mcp server:9000` | nginx container |
| **Kubernetes** | Deployment + Service | Kong, Envoy, nginx ingress |

## Observability

Every MCP request emits a structured JSON event:

```bash
mcpr --mcp :9000 --events
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
```

## CSP Handling

MCP Apps (ChatGPT Apps, Claude connectors) render widgets in sandboxed iframes with strict CSP. Every platform enforces it differently. mcpr handles this automatically:

- Scans widget HTML for `<script src>`, `<link href>`, `fetch()`, `WebSocket` references
- Adds missing domains to `connectDomains`, `resourceDomains`, `frameDomains`
- Removes conflicting `<meta>` CSP tags injected by build tools
- Adapts format per platform (ChatGPT needs `openai/widgetCSP`, Claude computes domain from SHA-256)
- Expands known CDN pairs (`fonts.googleapis.com` → also adds `fonts.gstatic.com`)

Zero config. Works on first proxy.

## Getting Started

### Proxy an MCP server

```bash
mcpr --mcp http://localhost:9000
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
extra_connect = ["api.stripe.com"]
extra_resources = ["cdn.example.com"]
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
- [ ] Per-tool health (calls, errors, p50/p95)
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
