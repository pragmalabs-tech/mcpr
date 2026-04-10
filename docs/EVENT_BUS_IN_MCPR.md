# Event Bus Design

## Overview

Every action in the mcpr proxy (request forwarded, session started, health check) produces a **ProxyEvent**. Events flow through a single pipeline — the **EventBus** — which fans them out to registered **sinks**. Each sink decides what to do with each event: print it, store it, send it to the cloud, or ignore it.

```
Proxy hot path (non-blocking)
  │
  │  event_bus.emit(ProxyEvent)
  ▼
EventBus ─── mpsc channel (10k buffer) ─── background tokio task
  │
  ├── StderrSink       → print to stderr (console)
  ├── SqliteSink       → write to local SQLite DB
  └── CloudSink        → batch POST to cloud.mcpr.app
       (future)
  ├── PrometheusSink   → increment counters / histograms
  ├── WebhookSink      → POST JSON to a URL
  └── FileSink         → append JSONL to rotating files
```

---

## Event Types

All events are variants of a single enum. Tagged JSON serialization (`#[serde(tag = "type")]`) so each event carries its type name.

### `ProxyEvent::Request`

Emitted when an MCP request completes (success or error). This is the primary observability event — every tool call, resource read, and passthrough request produces one.

| Field | Type | Description |
|-------|------|-------------|
| `id` | `String` | UUIDv4, unique per event |
| `ts` | `i64` | Unix milliseconds (UTC) |
| `proxy` | `String` | Proxy name (from config or derived from upstream URL) |
| `session_id` | `Option<String>` | MCP session ID from `mcp-session-id` header |
| `method` | `String` | HTTP method (POST, GET, DELETE) |
| `path` | `String` | Request path |
| `mcp_method` | `Option<String>` | MCP JSON-RPC method (tools/call, resources/read, etc.) |
| `tool` | `Option<String>` | Tool name for `tools/call` requests |
| `status` | `u16` | HTTP response status code |
| `latency_ms` | `u64` | Wall-clock ms: proxy received request to sent response |
| `upstream_ms` | `Option<u64>` | Time spent waiting for upstream response (ms) |
| `request_size` | `Option<u64>` | Request payload size in bytes |
| `response_size` | `Option<u64>` | Response payload size in bytes |
| `error_code` | `Option<String>` | JSON-RPC error code if response was an error |
| `error_msg` | `Option<String>` | Error message (truncated to 512 chars) |
| `note` | `String` | Classification: "rewritten", "passthrough", "sse", "error", "intercepted" |

### `ProxyEvent::SessionStart`

Emitted once per MCP session, when the proxy intercepts the `initialize` handshake and the upstream returns a successful response with a session ID.

| Field | Type | Description |
|-------|------|-------------|
| `session_id` | `String` | MCP session ID from the response |
| `proxy` | `String` | Proxy name |
| `ts` | `i64` | Unix milliseconds |
| `client_name` | `Option<String>` | From `clientInfo.name` (e.g., "claude-desktop") |
| `client_version` | `Option<String>` | From `clientInfo.version` (e.g., "1.2.0") |
| `client_platform` | `Option<String>` | Normalized: "claude", "chatgpt", "vscode", "cursor", "unknown" |

### `ProxyEvent::SessionEnd`

Emitted when a session is closed via a DELETE request with an `mcp-session-id` header.

| Field | Type | Description |
|-------|------|-------------|
| `session_id` | `String` | MCP session ID |
| `ts` | `i64` | Unix milliseconds |

### `ProxyEvent::Heartbeat`

Emitted every 10 seconds by the health check loop. Carries a snapshot of the proxy's current state.

| Field | Type | Description |
|-------|------|-------------|
| `ts` | `i64` | Unix milliseconds |
| `proxy` | `String` | Proxy name |
| `mcp_status` | `String` | "Connected", "Disconnected", "Not MCP" |
| `tunnel_status` | `String` | "Connected", "Disconnected", "Connecting" |
| `widgets_status` | `String` | "Connected", "Disconnected", "Unknown" |
| `uptime_secs` | `u64` | Seconds since proxy started |
| `request_count` | `u64` | Total requests handled |

---

## EventSink Trait

```rust
pub trait EventSink: Send + Sync {
    /// Process a single event. Must not block.
    fn on_event(&self, event: &ProxyEvent);

    /// Process a batch of events. Default calls on_event for each.
    /// Override for sinks that benefit from batching (SQL INSERT, HTTP POST).
    fn on_batch(&self, events: &[ProxyEvent]) { ... }

    /// Flush internal buffers. Called every ~5s and on graceful shutdown.
    fn flush(&self) {}

    /// Human-readable sink name.
    fn name(&self) -> &'static str;
}
```

**Location:** `mcpr-core/src/event.rs` — any crate can implement it.

**Contract:**

- `on_event` is called from a single background tokio task. It must not block. If the sink needs network or disk I/O, buffer internally (use an mpsc channel, `Vec`, or mutex-protected buffer) and flush in `flush()` or a background thread.
- `on_batch` is called when multiple events are available at once (up to 256). Sinks that benefit from batching (SQL transactions, HTTP POST with array body) should override this.
- `flush` is called periodically (~5s) and once on graceful shutdown. Sinks should drain any internal buffers here.

---

## EventBus

**Location:** `mcpr-cli/src/event_bus.rs`

The EventBus is the router. It owns a bounded mpsc channel and a background tokio task that reads events and fans them out to all registered sinks.

### Configuration

| Constant | Value | Description |
|----------|-------|-------------|
| `CHANNEL_CAPACITY` | 10,000 | Max buffered events before dropping |
| `BATCH_SIZE` | 256 | Max events per batch dispatch |
| `FLUSH_INTERVAL_MS` | 5,000 | Periodic flush interval |

### Lifecycle

```
EventBus::start(sinks)     → spawns background task, returns EventBusHandle
EventBus::emit(event)      → try_send to channel (non-blocking, drop if full)
EventBusHandle::shutdown()  → signal background task, drain remaining events, flush all sinks
```

### Backpressure

If the channel is full (10,000 events buffered), `emit()` silently drops the event. This is intentional — a busy proxy must never block on event processing. Dropped events mean the sinks can't keep up. In practice this should never happen at normal MCP request rates.

---

## Built-in Sinks

### StderrSink

**Location:** `mcpr-cli/src/stderr_sink.rs`  
**Listens to:** `Request` only

Prints a one-line summary per request to stderr. Supports two formats:

**Pretty** (default for terminals):
```
10:15:33 POST 200 1.0KB 142ms tools/call -> search_products /mcp
```

**JSON** (`--log-format json`):
```json
{"type":"request","id":"...","ts":1712345678000,"proxy":"localhost-9000","method":"POST","path":"/mcp","mcp_method":"tools/call","tool":"search_products","status":200,"latency_ms":142,"note":"rewritten"}
```

### SqliteSink

**Location:** `mcpr-integrations/src/store/sqlite_sink.rs`  
**Listens to:** `Request`, `SessionStart`, `SessionEnd`

Converts `ProxyEvent` variants into SQLite store operations:

| Event | Store Operation |
|-------|----------------|
| `Request` | INSERT into `requests` table + UPDATE session counters |
| `SessionStart` | INSERT into `sessions` table |
| `SessionEnd` | UPDATE `ended_at` on the session |
| `Heartbeat` | Ignored |

The SqliteSink wraps the `Store` engine which has its own background writer thread with batch flushing (200ms intervals). So the flow is: EventBus → SqliteSink.on_event() → Store.record() → mpsc channel → writer thread → SQLite.

### CloudSink

**Location:** `mcpr-integrations/src/emitter/cloud_sink.rs`  
**Listens to:** All events

Batches events and POSTs them to the cloud.mcpr.app ingest API. Has its own internal mpsc channel and background tokio task for buffering and retry.

| Config | Default | Description |
|--------|---------|-------------|
| `endpoint` | — | Full ingest URL |
| `token` | — | Project API token |
| `server` | — | Server slug (stamped on each event) |
| `batch_size` | 100 | Flush when buffer reaches this size |
| `flush_interval` | 5s | Flush on interval even if buffer isn't full |
| `on_flush` | — | Optional callback for sync status reporting |

**Retry:** 3 attempts with exponential backoff (1s, 2s, 4s). After 3 failures, events are dropped and the `on_flush` callback reports the failure.

---

## Writing a Custom Sink

Implement `EventSink` in any crate that depends on `mcpr-core`:

```rust
use mcpr_core::event::{EventSink, ProxyEvent};

pub struct WebhookSink {
    url: String,
    buffer: std::sync::Mutex<Vec<ProxyEvent>>,
}

impl EventSink for WebhookSink {
    fn on_event(&self, event: &ProxyEvent) {
        // Buffer events — don't do HTTP here (must not block).
        self.buffer.lock().unwrap().push(event.clone());
    }

    fn flush(&self) {
        let events: Vec<ProxyEvent> = {
            let mut buf = self.buffer.lock().unwrap();
            std::mem::take(&mut *buf)
        };
        if events.is_empty() { return; }

        // Fire-and-forget HTTP POST (spawn a task, don't block flush).
        let url = self.url.clone();
        tokio::spawn(async move {
            let _ = reqwest::Client::new()
                .post(&url)
                .json(&events)
                .send()
                .await;
        });
    }

    fn name(&self) -> &'static str { "webhook" }
}
```

Register it at startup in `main.rs`:

```rust
sinks.push(Box::new(WebhookSink {
    url: "https://hooks.example.com/mcpr".into(),
    buffer: Default::default(),
}));
let event_bus_handle = EventBus::start(sinks);
```

---

## Crate Boundaries

```
mcpr-core
  └── event.rs          ProxyEvent enum, EventSink trait, NoopSink

mcpr-integrations
  ├── store/sqlite_sink.rs    SqliteSink (ProxyEvent → SQLite)
  └── emitter/cloud_sink.rs   CloudSink (ProxyEvent → cloud API)

mcpr-cli
  ├── event_bus.rs       EventBus (channel + background task + fan-out)
  └── stderr_sink.rs     StderrSink (ProxyEvent → stderr)
```

- `mcpr-core` has no heavy dependencies (just `serde`). Any crate can implement `EventSink`.
- `mcpr-integrations` depends on `mcpr-core` + `rusqlite` + `reqwest` for the built-in sinks.
- `mcpr-cli` depends on both and wires everything together at startup.

---

## Design Decisions

**Why one enum instead of separate event types per sink?**  
Sinks need to see the full picture. A Prometheus sink wants `Request` events for counters AND `Heartbeat` events for gauges. A cloud sink wants everything. Separate types would force each sink to register for specific types, adding complexity.

**Why clone events for CloudSink?**  
CloudSink has its own internal buffer (mpsc channel). It receives `&ProxyEvent` from the EventBus and needs to own it for async HTTP posting. The clone cost (~200 bytes per event) is negligible compared to the network I/O.

**Why not use trait objects with downcasting?**  
`dyn Any` loses type safety. The concrete `ProxyEvent` enum gives compile-time exhaustiveness checking — if a new variant is added, every sink's `match` gets a compiler warning.

**Why channel capacity of 10,000?**  
At 1,000 requests/second, this is a 10-second buffer. More than enough to absorb any transient sink slowness. If the channel fills up, events are dropped — this is the correct behavior (proxy latency must never be affected by sink performance).

**Why BATCH_SIZE of 256?**  
Balances latency (don't hold events too long) with efficiency (amortize dispatch overhead). At ~200 bytes per event, 256 events is ~50KB of memory — negligible.
