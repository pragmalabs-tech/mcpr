# Schema Capture

mcpr passively captures what the upstream MCP server exposes — tools, resources, prompts, capabilities — by intercepting discovery responses as they flow through the proxy. No extra authentication, no active polling, zero latency added.

## Why

MCP servers behind OAuth can't be queried directly by the proxy. But every client that connects goes through the `initialize` → `tools/list` → `tools/call` flow. mcpr observes these responses to build a picture of the server's schema without needing its own credentials.

This enables:

- **Visibility** — know what your MCP server exposes without reading its source code
- **Change tracking** — detect when tools are added, removed, or modified
- **Stale detection** — know when the server says its schema changed but no client has fetched the update yet
- **Future: multi-server routing** — schema capture gives you the tool→server mapping for automatic routing

## What Gets Captured

| Response Method | What's Stored |
|-----------------|---------------|
| `initialize` | Server name, version, protocol version, declared capabilities |
| `tools/list` | Tool names, descriptions, `inputSchema`, `_meta` |
| `resources/list` | Resource URIs, names, descriptions, mimeType, `_meta` |
| `resources/templates/list` | Template URIs, names, descriptions |
| `prompts/list` | Prompt names, descriptions, arguments |

Additionally, `notifications/tools/list_changed` is detected in POST response bodies and marks the `tools/list` schema as stale.

## Capture Flow

```
Client                    mcpr                         MCP Server
  │                        │                               │
  │  initialize            │  initialize                   │
  │ ──────────────────────►│ ─────────────────────────────►│
  │                        │◄───────────────────────────── │
  │◄──────────────────────│                                │
  │                        │  CAPTURE: server name,        │
  │                        │  version, capabilities        │
  │                        │                               │
  │  tools/list            │  tools/list                   │
  │ ──────────────────────►│ ─────────────────────────────►│
  │                        │◄───────────────────────────── │
  │◄────────────────────── │                               │
  │                        │  CAPTURE: tool names,         │
  │                        │  schemas, diff vs previous    │
  │                        │                               │
  │  (server pushes)       │                               │
  │                        │◄ notifications/tools/         │
  │                        │  list_changed                 │
  │◄────────────────────── │                               │
  │                        │  MARK: tools/list stale       │
```

Capture happens **after** the response is forwarded to the client and **before** CSP rewriting — so the stored schema reflects the raw server response, not the proxy-modified version. The capture is emitted as a non-blocking event through the event bus, adding zero latency to the request path.

## Where It Happens in Code

### Protocol Layer (`mcpr-protocol/src/schema.rs`)

Pure MCP protocol logic with no HTTP or storage dependencies:

- `is_schema_method()` — determines which methods trigger capture
- `detect_page_status()` — detects MCP cursor-based pagination state
- `merge_pages()` — combines paginated responses into a single snapshot
- `diff_schema()` — compares two snapshots, returns granular changes (tool_added, tool_removed, tool_modified, etc.)

### Event Layer (`mcpr-core/src/event.rs`)

Two new `ProxyEvent` variants flow through the event bus:

- `SchemaCapture` — a discovery response was intercepted (carries the `result` JSON and pagination state)
- `SchemaStale` — `notifications/tools/list_changed` was detected

### Capture Point (`mcpr-cli/src/mcp_handler.rs`)

In `handle_mcp_post()`, between JSON parsing and `rewrite_response()`:

```
Request arrives → forward to upstream → receive response
  → parse JSON
  → emit SchemaCapture event (if schema method + successful response)
  → emit SchemaStale event (if notifications/tools/list_changed detected)
  → rewrite response (CSP)
  → send to client
```

### Storage Layer (`mcpr-integrations/src/store/`)

The SQLite writer thread (single OS thread, same as request/session storage) handles schema events:

1. **Hash** the payload (SHA-256)
2. **Compare** against the stored hash for that `(upstream_url, method)` pair
3. **Diff** if changed — calls `mcpr_protocol::schema::diff_schema()` to detect added/removed/modified items
4. **Write** — UPSERT the snapshot, INSERT change records

## Storage Schema

Two SQLite tables, added in schema version 2:

```sql
-- Latest snapshot per upstream server + MCP method.
-- One row per (upstream_url, method) pair, always the most recent capture.
CREATE TABLE server_schema (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    upstream_url  TEXT NOT NULL,
    method        TEXT NOT NULL,       -- 'initialize', 'tools/list', etc.
    payload       TEXT NOT NULL,       -- full JSON of the `result` field
    captured_at   INTEGER NOT NULL,    -- unix ms
    schema_hash   TEXT NOT NULL,       -- SHA-256 hex of payload
    UNIQUE(upstream_url, method)
);

-- Append-only change history.
-- Every schema change (initial capture, tool added/removed/modified, stale marker)
-- gets a row here.
CREATE TABLE schema_changes (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    upstream_url  TEXT NOT NULL,
    method        TEXT NOT NULL,
    change_type   TEXT NOT NULL,       -- see table below
    item_name     TEXT,                -- e.g. 'search_products', NULL for bulk changes
    old_hash      TEXT,
    new_hash      TEXT,
    detected_at   INTEGER NOT NULL     -- unix ms
);
```

### Change Types

| `change_type` | Meaning |
|----------------|---------|
| `initial` | First capture for this method |
| `tool_added` | A new tool appeared in `tools/list` |
| `tool_removed` | A tool disappeared from `tools/list` |
| `tool_modified` | A tool's definition changed (description, inputSchema, meta) |
| `resource_added` | A new resource appeared |
| `resource_removed` | A resource disappeared |
| `resource_modified` | A resource's definition changed |
| `resource_template_added` | A new resource template appeared |
| `resource_template_removed` | A resource template disappeared |
| `resource_template_modified` | A resource template's definition changed |
| `prompt_added` | A new prompt appeared |
| `prompt_removed` | A prompt disappeared |
| `prompt_modified` | A prompt's definition changed |
| `updated` | Payload changed but no named items differ (structural change, or `initialize`) |
| `stale` | Server sent `notifications/tools/list_changed` |

## Schema Status

Status is **computed** from captured data, not stored as a column:

| Status | Condition |
|--------|-----------|
| `unknown` | No schema captured for this upstream |
| `partial` | Some discovery methods captured but not all (e.g. have `tools/list` but no `initialize`) |
| `complete` | `initialize` + at least one list method captured |
| `stale` | A `stale` change record exists that is newer than the last capture for that method |

The proxy can't know the full schema until clients exercise all list methods. In practice, most MCP clients call `initialize` + `tools/list` on connect, so `complete` status is typical within seconds of the first connection.

## Pagination

MCP list methods support cursor-based pagination (rare but possible):

| Request `params.cursor` | Response `result.nextCursor` | State |
|---|---|---|
| absent | absent | **Complete** — single page, store directly |
| absent | present | **First page** — start buffering |
| present | present | **Middle page** — append to buffer |
| present | absent | **Last page** — merge all pages, then store |

The writer thread accumulates pages in memory and merges them (combining the array field across pages) before hashing and storing. If a client abandons pagination mid-way (requests page 1 but never page 2), the buffer is discarded after 60 seconds.

## CLI

```bash
# Show current schema
mcpr proxy schema

# Show change history
mcpr proxy schema --changes

# Show tool usage — highlight unused tools
mcpr proxy schema --unused
mcpr proxy schema --unused --since 30d

# Filter to one method
mcpr proxy schema --method tools/list

# Machine-readable
mcpr proxy schema --json
mcpr proxy schema --changes --json

# More history
mcpr proxy schema --changes --limit 100
```

### Unused Tools

`--unused` cross-references the captured `tools/list` schema with actual `tools/call` request logs. Tools that are listed by the server but never called by any client are highlighted:

```
TOOL USAGE — localhost-9000 — last 7d   2/5 unused

  TOOL                             CALLS   ERRORS          LAST CALLED  STATUS
  send_email                           0        0                never  unused
  internal_debug                       0        0                never  unused
  search_products                    847        3    2026-04-12 14:30  ok
  get_product                        312        0    2026-04-12 14:15  ok
  create_order                        89        8    2026-04-12 13:00  errors

  2 tools listed but never called in the last 7d.
```

This helps answer: "Are clients actually using what the server exposes?" — useful for identifying dead tools, understanding agent workflows, and cleaning up unused server capabilities.

## Design Decisions

**Why passive capture instead of active polling?**
The proxy doesn't have the server's auth credentials. Clients authenticate with the server through the proxy — we observe what flows through, but we don't initiate requests on behalf of clients. This is a security-conscious design: no tokens are stored or reused.

**Why capture before rewrite?**
The proxy rewrites CSP metadata in responses. We want the raw server schema, not the proxy-modified version.

**Why hash-based change detection?**
Computing a SHA-256 hash of the payload is cheaper than parsing and comparing JSON on every request. Only when the hash changes do we parse both payloads and diff them. For the common case (schema unchanged), this adds ~1 microsecond.

**Why diff in the writer thread?**
The writer thread is single-threaded and already has the DB connection. Doing the read-compare-write in one place avoids race conditions and keeps the proxy hot path free of any DB access.

**Why store the full payload?**
The full `result` JSON is stored so the CLI can pretty-print any format without the writer needing to understand every schema field. It also future-proofs against MCP spec additions (new fields in tool definitions, etc.).
