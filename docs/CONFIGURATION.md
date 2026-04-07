# Configuration Reference

mcpr uses `mcpr.toml` for configuration. It searches the current directory, then parent directories. CLI args and environment variables override config file values.

**Priority:** CLI args > environment variables > `mcpr.toml` > defaults

## Modes

mcpr runs in one of two modes:

| Mode | Trigger | Purpose |
|------|---------|---------|
| **Gateway** (default) | No `--relay` flag | Proxy + tunnel client for local MCP development |
| **Relay** | `--relay` flag or `mode = "relay"` in config | Tunnel relay server deployed on a VPS |

## Gateway Mode

### Minimal

```toml
mcp = "http://localhost:9000"
```

### Full example

```toml
# Upstream MCP server (required)
mcp = "http://localhost:9000/mcp"

# Widget source: URL (proxy to dev server) or file path (static serve)
widgets = "http://localhost:4444"

# Local proxy port (optional in tunnel mode -- picks random port if omitted)
port = 3000

# Disable tunnel -- local-only mode
no_tunnel = false

[tunnel]
# Relay server URL (default: https://tunnel.mcpr.app)
relay_url = "https://tunnel.mcpr.app"

# Persistent tunnel identity (auto-generated and saved on first run)
token = "90c74def-8fdc-4922-8702-44bc5cabf830"

# Fixed subdomain (optional -- derived from token if omitted)
subdomain = "myapp"

# Skip interactive claim flow (default: false)
anonymous = false

[csp]
# CSP rewriting mode: "extend" (default) or "override"
mode = "extend"

# Additional CSP domains to allow
domains = ["https://media.mcpr.app", "https://api.example.com"]

[events]
# Emit structured JSON events to stdout
enabled = true

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
```

### Field reference

| Field | CLI | Env | Description |
|-------|-----|-----|-------------|
| `mcp` | `--mcp` | | Upstream MCP server URL |
| `widgets` | `--widgets` | | Widget source: URL or file path |
| `port` | `--port` | | Local proxy port |
| `no_tunnel` | `--no-tunnel` | | Disable tunnel (local-only mode) |
| `[tunnel].relay_url` | `--relay-url` | `MCPR_RELAY_URL` | Relay server URL |
| `[tunnel].token` | | | Tunnel authentication token |
| `[tunnel].subdomain` | | | Fixed subdomain for tunnel |
| `[tunnel].anonymous` | | | Skip interactive claim flow (bool) |
| `[csp].mode` | `--csp-mode` | | `"extend"` or `"override"` |
| `[csp].domains` | `--csp` | | Extra CSP domains (repeatable via CLI) |
| `[events].enabled` | `--events` | | Emit structured JSON events to stdout |
| `[cloud].token` | `--cloud-token` | `MCPR_CLOUD_TOKEN` | Cloud sync token from mcpr.app |
| `[cloud].server` | `--cloud-server` | | Server slug for cloud routing |
| `[cloud].endpoint` | | | Custom cloud API endpoint |
| `[cloud].batch_size` | | | Events per batch |
| `[cloud].flush_interval_ms` | | | Flush interval in milliseconds |
| `[logging].file` | | | Enable JSONL file logging (bool) |
| `[logging].dir` | | | Directory for log files |
| `[logging].rotation` | | | Rotation: `"daily"` or `"size:50MB"` |

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

| Field | CLI | Env | Description |
|-------|-----|-----|-------------|
| `mode` | `--relay` | | Set to `"relay"` to run as relay server |
| `port` | `--port` | | Port the relay listens on |
| `[relay].domain` | `--relay-domain` | | Base domain for tunnel subdomains |
| `[relay].auth_provider` | `--auth-provider` | `MCPR_AUTH_PROVIDER` | External auth provider URL |
| `[relay].auth_provider_secret` | `--auth-provider-secret` | `MCPR_AUTH_PROVIDER_SECRET` | Shared secret for auth provider |
| `[[relay.tokens]]` | | | Static token entries (see below) |

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

- [Deploy Relay Server](DEPLOY_RELAY_SERVER.md) -- VPS setup, DNS, TLS, nginx/Caddy
- [Static Tokens](STATIC_TOKENS.md) -- practical guide for static token auth (team setup, CI/CD, demos)
- [Auth Provider](AUTH_PROVIDER.md) -- building an external auth provider API
