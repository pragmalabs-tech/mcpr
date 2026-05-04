<div align="center">

<img src="docs/assets/logo.png" alt="mcpr logo" width="120" />

# mcpr

[![CI](https://github.com/pragmalabs-tech/mcpr/actions/workflows/check.yml/badge.svg)](https://github.com/pragmalabs-tech/mcpr/actions/workflows/check.yml)
[![codecov](https://codecov.io/gh/pragmalabs-tech/mcpr/branch/main/graph/badge.svg)](https://codecov.io/gh/pragmalabs-tech/mcpr)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue)](LICENSE)

**Observability-first proxy for MCP servers.** A Rust proxy sits in front of your MCP app and records every JSON-RPC: per-tool latency, session traces, schema diffs, client breakdowns. It also help you to configuration csp policy and handle Auth (both oauth, api key) at the proxy layer.

</div>

## What mcpr does

mcpr sits in front of your MCP app and does three things, in order of how much work each one saves you:

1. **Observe**: records every `tools/call`, `resources/*`, and `prompts/*` request to a local SQLite store. Per-tool p50/p95/max latency, error rates, session traces, client breakdowns, and schema diffs over time. No instrumentation in your app.
2. **Route**: one upstream per proxy today. JSON-RPC classification, and CSP rewriting that emits the shape each AI client (ChatGPT, Claude, Copilot) expects.
3. **Authenticate** *(in progress)*: OAuth 2.1 and API key handling at the proxy layer. Your app receives a verified `x-user-id` header instead of implementing auth flows itself.

Running in front of [mcp.usestudykit.com/mcp](https://mcp.usestudykit.com/mcp) today.

## Quickstart

mcpr runs in two shapes:

- **Local development.** Install the CLI and point it at your MCP server.
- **Production.** Run the published Docker image as a sidecar via `docker compose` (or any container orchestrator).

### Local: install the CLI

```bash
curl -fsSL https://mcpr.app/install.sh | sh
```

Write a config and run the proxy in the foreground:

```bash
cat > mcpr.toml <<'EOF'
mcp  = "http://localhost:9000/mcp"
port = 3000
EOF

mcpr proxy run mcpr.toml
```

Clients connect to `http://localhost:3000`; mcpr forwards to `http://localhost:9000/mcp` and records every JSON-RPC for you to inspect later.

To stream events to the cloud dashboard, run `mcpr proxy setup` once.

### Production: docker compose

The published image (`ghcr.io/pragmalabs-tech/mcpr`) runs `mcpr proxy run` as PID 1. Drop it next to your MCP server in a compose file:

```yaml
services:
  mcp-server:
    image: your-mcp-server:latest
    expose:
      - "9000"

  mcpr:
    image: ghcr.io/pragmalabs-tech/mcpr:0.5
    restart: unless-stopped
    depends_on:
      - mcp-server
    ports:
      - "3000:3000"
    volumes:
      - ./mcpr.toml:/etc/mcpr/mcpr.toml:ro
      - mcpr-state:/var/lib/mcpr

volumes:
  mcpr-state:
```

```toml
# mcpr.toml (alongside the compose file)
mcp  = "http://mcp-server:9000"
port = 3000
```

`docker compose up -d`, then point clients at the host on port 3000. State persists in the `mcpr-state` volume across restarts.

Pin to `X.Y.Z` (or `X.Y` for the latest patch in a minor line) in production; `latest` is for evaluation.

#### Kubernetes / Helm

There is no official Helm chart yet. The image is Kubernetes-ready out of the box: non-root UID 10001, SIGTERM forwarded by `tini`, `mcpr proxy run` is PID 1 and drains in-flight requests on shutdown. Use any standard `Deployment` manifest or a generic chart (bitnami/common, k8s-at-home/app-template):

- Bind-mount `mcpr.toml` from a `ConfigMap` to `/etc/mcpr/mcpr.toml`.
- Persist `/var/lib/mcpr` with a `PersistentVolumeClaim` (UID 10001 must own it; set `fsGroup: 10001`).
- Expose port 3000 via a `Service`.

See [docs/DOCKER.md](docs/DOCKER.md) for volumes, environment variables, signal behavior, and non-root settings.

---

## Observe

Every JSON-RPC request that flows through mcpr lands in `~/.mcpr/store.db`: tool name, latency, status, error code, request/response size, session ID, server/client info captured on `initialize`, and full `tools/list` / `resources/list` / `prompts/list` responses for schema tracking.

### Local SQLite

The store at `~/.mcpr/store.db` is the source of truth. Inspect it directly with the `sqlite3` CLI for ad-hoc analysis, or use:

```bash
mcpr store stats             # row counts, oldest/newest events, db size
mcpr store vacuum            # delete old records and reclaim disk
```

---

## Route

Each proxy instance fronts one upstream MCP app. mcpr classifies requests by JSON-RPC shape: MCP methods go to the backend; anything else is forwarded upstream as-is.

```toml
mcp = "http://localhost:9000"
```

To proxy multiple MCP servers, write one `mcpr.toml` per upstream and launch each with `mcpr proxy run <path>`.

### Widget CSP

mcpr applies widget CSP in both shapes (the legacy OpenAI per-widget format and the current MCP standard), so a single config block works for both Claude and ChatGPT.

```toml
[csp]
# Public host the proxy is reachable on. Written into the OpenAI
# `widgetDomain` field. `_meta.ui.domain` is left to Claude, which
# derives that field from the proxy URL itself and rejects values
# supplied by anything in front of it.
domain = "widgets.example.com"

# Lands in `connect-src` (fetch / WebSocket / EventSource targets).
# `extend` merges with whatever the upstream MCP server declared;
# `replace` ignores upstream.
[csp.connectDomains]
domains = ["api.example.com"]
mode    = "extend"

# Lands in `script-src`, `style-src`, `img-src`, `font-src`, `media-src`:
# one bucket for everything the widget loads. Same merge semantics.
[csp.resourceDomains]
domains = ["cdn.example.com"]
mode    = "extend"

# Lands in `frame-src` (nested iframes). Defaults to `replace` so
# upstream cannot silently widen this directive.
[csp.frameDomains]
domains = []
mode    = "replace"

# Per-widget override, matched by URI pattern (glob). Only the
# payment widget gets `connect-src` to Stripe.
[[csp.widget]]
match              = "ui://widget/payment*"
connectDomains     = ["api.stripe.com"]
connectDomainsMode = "extend"
```

---

## Authenticate

*In progress.* mcpr will handle MCP OAuth 2.1 and API key auth at the proxy layer, so your MCP app receives a verified `x-user-id` header instead of implementing auth flows itself. Planned config:

```toml
[auth]
mode = "oauth2.1"           # or "api_key"
provider = "google"         # google, github, bring-your-own
```

Track progress in the [Roadmap](#roadmap) below. Open an issue if your provider isn't covered.

---

## Reference

- Configuration: [docs/proxy/PROXY_CONFIGURATION.md](docs/proxy/PROXY_CONFIGURATION.md) (upstream URL, port, CSP, cloud sync, limits)
- CLI: [docs/CLI.md](docs/CLI.md) (`proxy run`, `proxy setup`, `store stats/vacuum`, `validate`, `version`)
- Docker: [docs/DOCKER.md](docs/DOCKER.md) (volumes, health probes, compose/Kubernetes)
- Bench harness: [benches/README.md](benches/README.md) (initialize + tools/call latency, direct vs proxied)

---

## Roadmap

**Observability**
- [x] Per-tool metrics (calls, error%, p50, p95, max, request/response size)
- [x] Request logs, session tracking, AI client tracking
- [x] Schema capture with change tracking
- [x] Cloud dashboard sync ([cloud.mcpr.app](https://cloud.mcpr.app))

**Routing & Network**
- [x] JSON-RPC routing (single upstream per proxy)
- [x] CSP rewriting
- [ ] Multi-upstream routing from one port

**Auth**
- [ ] OAuth 2.1 for standard providers
- [ ] OAuth 2.1 for legacy (non-standard) auth
- [ ] API token auth
- [ ] Multiple auth modes per server

**Security**
- [ ] Per-tool access control
- [ ] Rate limiting and circuit breaker
- [ ] IP whitelist

## License

Apache 2.0
