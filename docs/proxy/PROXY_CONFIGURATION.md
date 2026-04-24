# Configuration Reference

`mcpr.toml` is the single source of truth for **how the proxy behaves** — where to route traffic, how to handle CSP, resource limits, tunnel settings, cloud sync, and storage. It declares proxy behavior; it does not manage the daemon process (that's the [CLI](CLI.md)).

mcpr searches the current directory, then parent directories, for `mcpr.toml`.

## Modes

mcpr runs in one of two modes, set in `mcpr.toml`:

| Mode | Config | Purpose |
|------|--------|---------|
| **Gateway** (default) | `mode` omitted or `mode = "gateway"` | Proxy + tunnel client for MCP development and production |
| **Relay** | `mode = "relay"` | Tunnel relay server deployed on a VPS |

## Gateway Mode

The proxy does not serve widget HTML or static assets itself — that path was removed to match the MCP spec. CSP rewriting of upstream responses still applies; see [CSP](CSP.md).

### Minimal

```toml
mcp = "http://localhost:9000"
```

### Full example

```toml
# Upstream MCP server (required)
mcp = "http://localhost:9000/mcp"

# Local proxy port (optional in tunnel mode -- picks random port if omitted)
port = 3000

# Disable tunnel -- local-only mode (set to true for production)

[tunnel]
# Enable tunnel for a public URL (default: false)
# enabled = true

# Relay server URL (default: https://tunnel.mcpr.app)
relay_url = "https://tunnel.mcpr.app"

# Tunnel authentication token (register at https://mcpr.app to get one)
token = "90c74def-8fdc-4922-8702-44bc5cabf830"

# Fixed subdomain (optional -- derived from token if omitted)
subdomain = "myapp"

# CSP — declare once, emitted to every widget meta in both shapes.
# See docs/proxy/CSP.md for the full reference, including per-widget overrides.
[csp]
# Bare public host (no scheme). Feeds `openai/widgetDomain` and the proxy URL
# injected into widget CSP. When unset, falls back to the tunnel URL; in
# local-only mode (no tunnel, no override), injection is suppressed rather
# than writing `localhost` into widget config.
publicWidgetDomain = "widgets.example.com"

[csp.connectDomains]
domains = ["https://api.example.com"]
mode    = "extend"

[csp.resourceDomains]
domains = ["https://media.mcpr.app"]
mode    = "extend"

[cloud]
# Cloud sync token (from mcpr.app project settings)
token = "mcpr_xxxxxxxx"

# Server slug (matches server name in your mcpr.app project)
server = "my-server"

# Custom API endpoint (default: https://api.mcpr.app)
# endpoint = "https://api.mcpr.app"

# Events per batch
# batch_size = 100

# Flush interval in milliseconds
# flush_interval_ms = 5000

[logging]
# Enable JSONL file logging
file = true

# Directory for log files (default: "./logs")
dir = "./logs"

# Rotation strategy: "daily" or "size:50MB" (default: "daily")
rotation = "daily"

[store]
# Enable request storage (default: true)
# When false, no SQLite database is created and CLI query commands are unavailable.
# enabled = true

# Override the database file path (default: platform-specific)
# Linux:   ~/.local/share/mcpr/mcpr.db
# macOS:   ~/Library/Application Support/mcpr/mcpr.db
# path = "/var/lib/mcpr/requests.db"

# Proxy name written to every stored request (default: derived from mcp URL)
# Used by `mcpr proxy logs <name>`, `mcpr proxy status <name>`, etc.
# name = "api-server"
```

### Field reference

| Field | Description |
|-------|-------------|
| `mcp` | Upstream MCP server URL |
| `port` | Local proxy port |
| `[tunnel].enabled` | Enable tunnel for public URL (default: false) |
| `[tunnel].relay_url` | Relay server URL |
| `[tunnel].token` | Tunnel authentication token (from mcpr.app) |
| `[tunnel].subdomain` | Fixed subdomain for tunnel |
| `[csp].publicWidgetDomain` | Bare public host for `openai/widgetDomain` and CSP injection — see [CSP](CSP.md) |
| `[csp.*]` | Widget CSP declaration — see [CSP](CSP.md) |
| `[cloud].token` | Cloud sync token from mcpr.app |
| `[cloud].server` | Server slug for cloud routing |
| `[cloud].endpoint` | Custom cloud API endpoint |
| `[cloud].batch_size` | Events per batch |
| `[cloud].flush_interval_ms` | Flush interval in milliseconds |
| `[logging].file` | Enable JSONL file logging (bool) |
| `[logging].dir` | Directory for log files |
| `[logging].rotation` | Rotation: `"daily"` or `"size:50MB"` |
| `[store].enabled` | Enable SQLite request storage (default: true) |
| `[store].path` | Override database file path |
| `[store].name` | Proxy name for stored requests |

## Relay Mode

### Minimal (open -- no auth)

```toml
mode = "relay"
port = 8081

[relay]
domain = "tunnel.yourdomain.com"
```

### With static tokens

```toml
mode = "relay"
port = 8081

[relay]
domain = "tunnel.yourdomain.com"

[[relay.tokens]]
token = "mcpr_abc123"
subdomains = ["myapp", "myapp-*"]

[[relay.tokens]]
token = "mcpr_def456"
subdomains = ["other-app", "other-app-*"]
```

### With auth provider

```toml
mode = "relay"
port = 8081

[relay]
domain = "tunnel.yourdomain.com"
auth_provider = "https://auth.yourdomain.com"
auth_provider_secret = "your-shared-secret-here"
```

### Field reference

| Field | Description |
|-------|-------------|
| `mode` | Set to `"relay"` to run as relay server |
| `port` | Port the relay listens on |
| `[relay].domain` | Base domain for tunnel subdomains |
| `[relay].auth_provider` | External auth provider URL |
| `[relay].auth_provider_secret` | Shared secret for auth provider |
| `[[relay.tokens]]` | Static token entries (see below) |

### Auth modes

The relay supports three auth modes (pick one):

| Mode | Config | When to use |
|------|--------|-------------|
| **Open** | No tokens, no auth_provider | Local dev, testing |
| **Static tokens** | `[[relay.tokens]]` entries | Small team, simple setup |
| **Auth provider** | `[relay].auth_provider` URL | Dynamic token management at scale |

Priority: static tokens > auth provider > open.

### Static token format

```toml
[[relay.tokens]]
token = "mcpr_abc123"           # the token clients use
subdomains = ["myapp", "myapp-*"]  # allowed subdomain patterns
```

### Subdomain patterns

Patterns support glob-style `*` wildcard:

| Pattern | Matches | Does not match |
|---------|---------|----------------|
| `myapp` | `myapp` | `myapp-dev` |
| `myapp-*` | `myapp-dev`, `myapp-feat-123` | `myapp` |
| `*-preview` | `feat-preview`, `hotfix-preview` | `preview` |
| `pr-*-acme` | `pr-123-acme`, `pr-abc-acme` | `pr-123` |
| `*` | anything | |

## Resource Limits & Timeouts

These apply to both gateway and relay modes.

```toml
# Max request body size in bytes (default: 5 MB)
max_request_body_size = 5242880

# Max response body size in bytes (default: 10 MB)
max_response_body_size = 10485760

# Max concurrent upstream connections (default: 100)
max_concurrent_upstream = 100

# Connect timeout in seconds (default: 5)
connect_timeout = 5

# Request timeout in seconds (default: 30)
request_timeout = 30
```

| Field | Default | Description |
|-------|---------|-------------|
| `max_request_body_size` | `5242880` (5 MB) | Reject inbound requests larger than this (413) |
| `max_response_body_size` | `10485760` (10 MB) | Reject upstream responses larger than this (502) |
| `max_concurrent_upstream` | `100` | Max in-flight requests to upstream (semaphore) |
| `connect_timeout` | `5` (seconds) | TCP connect timeout to upstream |
| `request_timeout` | `30` (seconds) | Total request timeout including response |

## Backward Compatibility

Legacy flat fields from older config files are still supported:

| Legacy field | New location |
|-------------|--------------|
| `relay_domain` | `[relay].domain` |
| `relay_url` | `[tunnel].relay_url` |
| `tunnel_token` | `[tunnel].token` |
| `tunnel_subdomain` | `[tunnel].subdomain` |

The new grouped format is recommended for new configs. See [`config_examples/`](../config_examples/) for templates.

## Related docs

- [CSP](CSP.md) -- widget CSP: directives, modes, per-widget overrides
- [Deploy Relay Server](DEPLOY_RELAY_SERVER.md) -- VPS setup, DNS, TLS, nginx/Caddy
- [Static Tokens](STATIC_TOKENS.md) -- practical guide for static token auth (team setup, CI/CD, demos)
- [Auth Provider](AUTH_PROVIDER.md) -- building an external auth provider API
