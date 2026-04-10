# Writing a Custom Event Sink

mcpr uses an event bus to route proxy events to sinks. You can add your own sink to send events to any destination — Prometheus, Datadog, a webhook, a file, Kafka, etc.

## Quick Start

```rust
use mcpr_core::event::{EventSink, ProxyEvent};

pub struct MySink;

impl EventSink for MySink {
    fn on_event(&self, event: &ProxyEvent) {
        match event {
            ProxyEvent::Request(e) => {
                println!("{} {} {}ms", e.mcp_method.as_deref().unwrap_or(&e.method), e.status, e.latency_ms);
            }
            _ => {} // ignore other event types
        }
    }

    fn name(&self) -> &'static str { "my-sink" }
}
```

Register it at startup:

```rust
sinks.push(Box::new(MySink));
let handle = EventBus::start(sinks);
```

That's it. Your sink receives every event the proxy emits.

## The EventSink Trait

```rust
// Defined in mcpr-core/src/event.rs

pub trait EventSink: Send + Sync {
    fn on_event(&self, event: &ProxyEvent);

    fn on_batch(&self, events: &[ProxyEvent]) {
        for event in events {
            self.on_event(event);
        }
    }

    fn flush(&self) {}

    fn name(&self) -> &'static str;
}
```

### Rules

1. **`on_event` must not block.** It's called from a single background tokio task. If you block here, you block ALL sinks.

2. **Buffer internally, flush externally.** If your sink needs I/O (HTTP, disk, network), buffer events in memory and do the I/O in `flush()` or a background thread/task.

3. **`flush()` is called every ~5 seconds and once on graceful shutdown.** This is your chance to drain buffers. After the final `flush()`, the process exits — anything not flushed is lost.

4. **`on_batch` is optional.** Override it if your sink benefits from batching (SQL INSERT with multiple rows, HTTP POST with array body). Default implementation calls `on_event` for each.

5. **Filter by variant.** You receive ALL events. Use `match` to pick the ones you care about.

## Event Types

```rust
pub enum ProxyEvent {
    Request(RequestEvent),       // MCP request completed
    SessionStart(SessionStartEvent), // initialize handshake
    SessionEnd(SessionEndEvent),     // session closed
    Heartbeat(HeartbeatEvent),       // periodic health snapshot
}
```

### Which events to handle

| Sink type | Request | SessionStart | SessionEnd | Heartbeat |
|-----------|---------|-------------|------------|-----------|
| Metrics (Prometheus) | yes (counters, histograms) | yes (gauge) | yes (gauge) | yes (uptime gauge) |
| Alerting (PagerDuty) | yes (error rate) | no | no | yes (health) |
| Log file (JSONL) | yes | yes | yes | no |
| Webhook | yes | yes | yes | depends |
| Analytics (Mixpanel) | yes | yes | no | no |

### RequestEvent fields

The most important event. Every tool call, resource read, and passthrough produces one.

```rust
pub struct RequestEvent {
    pub id: String,              // UUIDv4
    pub ts: i64,                 // unix ms
    pub proxy: String,           // proxy name
    pub session_id: Option<String>,
    pub method: String,          // HTTP method
    pub path: String,            // request path
    pub mcp_method: Option<String>, // tools/call, resources/read, etc.
    pub tool: Option<String>,    // tool name
    pub status: u16,             // HTTP status
    pub latency_ms: u64,         // wall-clock ms
    pub upstream_ms: Option<u64>,// upstream network time
    pub request_size: Option<u64>,
    pub response_size: Option<u64>,
    pub error_code: Option<String>,
    pub error_msg: Option<String>,
    pub note: String,            // "rewritten", "passthrough", "error"
}
```

All event types derive `Serialize`, so you can serialize to JSON with `serde_json::to_string(event)`.

## Patterns

### Pattern 1: Fire-and-forget (simple)

For sinks where losing some events is acceptable (metrics, analytics):

```rust
impl EventSink for MetricsSink {
    fn on_event(&self, event: &ProxyEvent) {
        if let ProxyEvent::Request(e) = event {
            self.counter.inc();
            self.histogram.observe(e.latency_ms as f64);
        }
    }

    fn name(&self) -> &'static str { "metrics" }
}
```

### Pattern 2: Buffer + flush (batched I/O)

For sinks that do network or disk I/O:

```rust
pub struct WebhookSink {
    url: String,
    buffer: Mutex<Vec<ProxyEvent>>,
}

impl EventSink for WebhookSink {
    fn on_event(&self, event: &ProxyEvent) {
        // Don't do HTTP here — just buffer.
        self.buffer.lock().unwrap().push(event.clone());
    }

    fn flush(&self) {
        let events: Vec<ProxyEvent> = {
            let mut buf = self.buffer.lock().unwrap();
            std::mem::take(&mut *buf)
        };
        if events.is_empty() { return; }

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

### Pattern 3: Internal channel (async background worker)

For sinks that need their own async loop (retry, backoff, connection management):

```rust
pub struct KafkaSink {
    tx: mpsc::Sender<ProxyEvent>,
}

impl KafkaSink {
    pub fn new(brokers: &str, topic: &str) -> Self {
        let (tx, rx) = mpsc::channel(1000);
        tokio::spawn(kafka_worker(rx, brokers.to_string(), topic.to_string()));
        Self { tx }
    }
}

impl EventSink for KafkaSink {
    fn on_event(&self, event: &ProxyEvent) {
        let _ = self.tx.try_send(event.clone()); // non-blocking
    }

    fn name(&self) -> &'static str { "kafka" }
}

async fn kafka_worker(mut rx: mpsc::Receiver<ProxyEvent>, brokers: String, topic: String) {
    // Connect to Kafka, consume events from rx, produce to topic.
    // Handle reconnection, batching, etc. in this loop.
}
```

This is the same pattern the built-in `CloudSink` uses.

### Pattern 4: Override on_batch (SQL, bulk API)

When inserting multiple rows at once is faster than one at a time:

```rust
impl EventSink for PostgresSink {
    fn on_event(&self, event: &ProxyEvent) {
        // Single insert — fallback for small batches.
        self.insert_one(event);
    }

    fn on_batch(&self, events: &[ProxyEvent]) {
        // Bulk insert — much faster for large batches.
        self.insert_many(events);
    }

    fn name(&self) -> &'static str { "postgres" }
}
```

## Dependency

Add `mcpr-core` to your `Cargo.toml`:

```toml
[dependencies]
mcpr-core = { version = "0.3", features = [] }
```

`mcpr-core` only depends on `serde` — no heavy frameworks.

## How Events Flow

```
mcp_handler.rs
  │  state.event_bus.emit(ProxyEvent::Request(...))
  │  state.event_bus.emit(ProxyEvent::SessionStart(...))
  ▼
EventBus (mcpr-cli/src/event_bus.rs)
  │  mpsc channel, capacity 10,000
  │  background tokio task
  │  batches up to 256 events
  ▼
for sink in sinks {
    if batch.len() == 1 {
        sink.on_event(&batch[0]);
    } else {
        sink.on_batch(&batch);  // default: calls on_event for each
    }
}
  │  every 5 seconds:
  │  for sink in sinks { sink.flush(); }
  │
  │  on shutdown:
  │  drain channel → dispatch remaining → flush all → exit
```

## What happens if my sink is slow?

The EventBus calls sinks **synchronously** in the background task. If your `on_event` takes 100ms, that's 100ms before the next event is dispatched. Other sinks wait too.

To avoid this:
- Buffer events in `on_event`, do I/O in `flush()` or a spawned task.
- Use the internal channel pattern (Pattern 3) for anything with network I/O.

If the EventBus channel fills up (10,000 events), new events are **silently dropped**. The proxy never blocks.

## Testing your sink

```rust
#[test]
fn my_sink_handles_request() {
    let sink = MySink::new();

    let event = ProxyEvent::Request(RequestEvent {
        id: "test-1".into(),
        ts: 1700000000000,
        proxy: "test".into(),
        session_id: None,
        method: "POST".into(),
        path: "/mcp".into(),
        mcp_method: Some("tools/call".into()),
        tool: Some("search".into()),
        status: 200,
        latency_ms: 42,
        upstream_ms: Some(40),
        request_size: Some(100),
        response_size: Some(200),
        error_code: None,
        error_msg: None,
        note: "rewritten".into(),
    });

    sink.on_event(&event);
    sink.flush();

    // Assert your sink did the right thing.
}
```
