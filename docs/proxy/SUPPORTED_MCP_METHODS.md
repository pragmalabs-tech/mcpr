# MCP Method Support

mcpr is a protocol-aware reverse proxy that sits before your MCP server. It parses
every JSON-RPC 2.0 message, classifies the MCP method, and extracts metadata for
logging, rewriting, and observability.

This document covers which MCP spec methods (2025-03-26) are recognized and what
the proxy does with each one.

## Supported Methods

### Lifecycle

| Method | Direction | Type | Proxy Behavior |
|--------|-----------|------|----------------|
| `initialize` | client->server | request | Extracts `clientInfo` (name, version, platform) to identify the session. Tracks session state transition to `Initialized`. **Schema capture**: stores server name, version, protocol version, and declared capabilities. |
| `notifications/initialized` | client->server | notification | Transitions session state to `Active`. |
| `ping` | bidirectional | request | Forwarded as-is. Logged. |

### Tools

| Method | Direction | Type | Proxy Behavior |
|--------|-----------|------|----------------|
| `tools/list` | client->server | request | **CSP rewriting**: rewrites `meta` on each tool in the response. **Schema capture**: stores tool names, descriptions, and input schemas with change tracking. |
| `tools/call` | client->server | request | **CSP rewriting**: rewrites `meta` on the response. Extracts **tool name** (`params.name`) for log detail. |
| `notifications/tools/list_changed` | server->client | notification | Classified and logged. **Schema capture**: marks the cached `tools/list` schema as stale. |

### Resources

| Method | Direction | Type | Proxy Behavior |
|--------|-----------|------|----------------|
| `resources/list` | client->server | request | **CSP rewriting**: rewrites `meta` on each resource in the response. **Schema capture**: stores resource URIs, names, and descriptions. |
| `resources/templates/list` | client->server | request | **CSP rewriting**: rewrites `meta` on each template in the response (`result.resourceTemplates[]`). **Schema capture**: stores template URIs and descriptions. |
| `resources/read` | client->server | request | **CSP rewriting**: rewrites `meta` on each content item. Extracts **resource URI** (`params.uri`) for log detail. HTML text content is never modified. |
| `resources/subscribe` | client->server | request | Forwarded as-is. Logged. |
| `resources/unsubscribe` | client->server | request | Forwarded as-is. Logged. |

### Prompts

| Method | Direction | Type | Proxy Behavior |
|--------|-----------|------|----------------|
| `prompts/list` | client->server | request | Forwarded as-is. Logged. **Schema capture**: stores prompt names, descriptions, and arguments. |
| `prompts/get` | client->server | request | Extracts **prompt name** (`params.name`) for log detail. |

### Utility

| Method | Direction | Type | Proxy Behavior |
|--------|-----------|------|----------------|
| `logging/setLevel` | client->server | request | Forwarded as-is. Logged. |
| `completion/complete` | client->server | request | Forwarded as-is. Logged. |
| `notifications/cancelled` | bidirectional | notification | Extracts **requestId** for log detail. Supports both string and numeric IDs. |
| `notifications/progress` | bidirectional | notification | Extracts **progressToken** for log detail. Supports both string and numeric tokens. |

## Not Yet Supported

These MCP spec methods are forwarded as passthrough traffic. They are not
classified into named variants and show as `Unknown` or generic `Notification`
in logs.

| Method | Direction | Type | Why Skipped |
|--------|-----------|------|-------------|
| `sampling/createMessage` | server->client | request | Server-to-client request. Rare in HTTP transports. |
| `roots/list` | server->client | request | Server-to-client request. Rare in HTTP transports. |
| `notifications/roots/list_changed` | client->server | notification | Low observability value for now. |
| `notifications/resources/list_changed` | server->client | notification | Low observability value for now. |
| `notifications/resources/updated` | server->client | notification | Low observability value for now. |
| `notifications/prompts/list_changed` | server->client | notification | Low observability value for now. |
| `notifications/message` | server->client | notification | Server log forwarding. Low proxy-level value. |

## How Classification Works

Every JSON-RPC 2.0 message passing through the proxy is parsed and classified:

1. **Parse**: The raw body is parsed as JSON-RPC 2.0 (single or batch).
2. **Classify**: The `method` string is matched against known constants to produce
   an `McpMethod` enum value.
3. **Extract detail**: For methods like `tools/call`, `resources/read`, and
   `notifications/cancelled`, the proxy extracts a short identifier from `params`
   for logging.
4. **Rewrite** (response path): For methods that return widget metadata, CSP
   domain arrays are rewritten to route through the proxy.

Unknown methods are still forwarded — the proxy never blocks traffic. They just
appear as `Unknown` in observability output.

## What Gets Logged

For every request, the proxy records:

| Field | Source |
|-------|--------|
| `mcp_method` | Classified method string (e.g. `tools/call`) |
| `tool` | Extracted detail: tool name, resource URI, prompt name, requestId, or progressToken |
| `error_code` | JSON-RPC error code from the response (if any) |
| `error_msg` | Error message (truncated to 512 chars) |
| `session_id` | MCP session ID from the `mcp-session-id` header |

Session-level metadata (from `initialize`):

| Field | Source |
|-------|--------|
| `client_name` | `params.clientInfo.name` |
| `client_version` | `params.clientInfo.version` |
| `client_platform` | Normalized: `claude`, `chatgpt`, `vscode`, `cursor`, or `unknown` |
