# Configuration Reference

`mcpr.toml` is the single source of truth for **how the proxy behaves** - where to route traffic, how to handle CSP, resource limits, tunnel settings, and cloud sync. It declares proxy behavior; it does not manage the daemon process (that's the [CLI](CLI.md)).

mcpr searches the current directory, then parent directories, for `mcpr.toml`.

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
# Bare public host (no scheme). Feeds the `openai/widgetDomain` meta
# field and the proxy URL injected into widget CSP. `_meta.ui.domain`
# is left untouched: Claude derives that field from the proxy URL and
# rejects values supplied by anything in front of it. When unset, falls
# back to the tunnel URL; in local-only mode (no tunnel, no override),
# injection is suppressed rather than writing `localhost` into widget
# config.
domain = "widgets.example.com"

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
| `[csp].domain` | Bare public host for `openai/widgetDomain` and CSP injection. `_meta.ui.domain` is left to Claude, which derives it from the proxy URL — see [CSP](CSP.md) |
| `[csp.*]` | Widget CSP declaration — see [CSP](CSP.md) |
| `[cloud].token` | Cloud sync token from mcpr.app |
| `[cloud].server` | Server slug for cloud routing |
| `[cloud].endpoint` | Custom cloud API endpoint |
| `[cloud].batch_size` | Events per batch |
| `[cloud].flush_interval_ms` | Flush interval in milliseconds |
| `drain_timeout` | Seconds to wait for in-flight requests on SIGTERM (default: 30) |
| `admin_bind` | Admin server bind address (default: `127.0.0.1:9901`) |

## Resource Limits & Timeouts

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

## Examples

A working config sits at [`examples/weather-app/mcpr.toml`](../../examples/weather-app/mcpr.toml).

The legacy CSP flat shape (`csp.mode` + `csp.domains`) is still parsed and folded into `connectDomains` and `resourceDomains`, but the per-directive form is preferred.

## Related docs

- [CSP](CSP.md) -- widget CSP: directives, modes, per-widget overrides
