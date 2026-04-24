# Proxy Pipeline

The mcpr proxy is a routing + observability layer for MCP traffic. Every HTTP request goes through the same per-request pipeline: content-based intake, an ordered request middleware chain, a pure router, a transport that is the only component touching the network, an ordered response middleware chain, and a final emit stage that writes one `RequestEvent` to the event bus.

This document describes what the pipeline is, the types that flow through it, and the middleware registered by default.

## Scope

mcpr forwards JSON-RPC between MCP clients (hosts like Claude Desktop, Cursor, VS Code) and upstream MCP servers. It does three things:

- Routes: picks the upstream and chooses whether to buffer or stream the response.
- Observes: emits a `RequestEvent` per request, tracks sessions, captures tool/resource/prompt schemas, records health.
- Rewrites: mutates upstream widget CSP directives so widgets loaded through the proxy reach the resources they need.

It does not serve static assets, render HTML, host local content, or support JSON-RPC batching. Traffic that doesn't belong to the MCP control plane is forwarded raw or rejected.

## Principles

- **One sum type per boundary.** `Request`, `Route`, `Response` are each a single enum. Stages pattern-match.
- **Pure router, I/O in transport.** The router is a function of `(Request, Config)`. Only the transport speaks to the network.
- **Variant-aware middleware.** Each middleware declares which variants it applies to; the driver runs every middleware against every response and the middleware decides whether to act.
- **Content-based intake.** Classification reads method, headers, and body — never a hard-coded URL path. Mount paths are user-configurable.
- **Two-stage parse.** Intake parses only the JSON-RPC envelope (cheap). Typed params/results deserialize lazily, only in middlewares that need them.
- **Envelope is data.** Whether a buffered MCP response is SSE-framed is a field on the response value, resolved once at the final seal stage.
- **Streamable HTTP is primary.** Legacy HTTP+SSE is supported as a byte passthrough, not a first-class code path.

## Layers

```
 HTTP (axum, with tower edge layers)
   │
   ▼
 ┌──────────────────────────┐
 │ Intake (content-first)   │   Axum fallback → Request enum.
 │  raw request → Request   │   POST + JSON-RPC → Mcp. GET + SSE Accept
 └──────────────────────────┘   → Mcp (legacy). DELETE + session-id →
   │                            Mcp (synthetic). Else → Raw.
   ▼
 ┌──────────────────────────┐
 │ Request middleware chain │   Ordered Vec<Box<dyn RequestMiddleware>>.
 │                          │   Short-circuit skips router + transport
 └──────────────────────────┘   and jumps to the response chain.
   │
   ▼
 ┌──────────────────────────┐
 │ Router (pure)            │   fn route(&Request, &Context) -> Route.
 └──────────────────────────┘   Buffer-policy table lives here.
   │
   ▼
 ┌──────────────────────────┐
 │ Transport                │   The only layer touching the network.
 │                          │   reqwest errors → Response::Upstream502.
 └──────────────────────────┘
   │
   ▼
 ┌──────────────────────────┐
 │ Response middleware chain│   Mutates McpMessage in place, records
 │                          │   session state, ingests schema, rewrites
 └──────────────────────────┘   URLs, seals envelope.
   │
   ▼
 ┌──────────────────────────┐
 │ Emit                     │   Builds one RequestEvent and writes it
 │                          │   to the event bus.
 └──────────────────────────┘
   │
   ▼
 IntoResponse → axum::Response
```

The axum edge wraps the pipeline in `DefaultBodyLimit`, `tower::limit::ConcurrencyLimitLayer`, `tower_http::timeout::TimeoutLayer` (504 on expiry), `tower_http::trace::TraceLayer`, and `CorsLayer`.

## Types

Spec types (`JsonRpcEnvelope`, `McpMessage`, method enums) live in `mcpr-core::protocol::jsonrpc` and `mcpr-core::protocol::mcp` — that's the pure MCP spec layer. Proxy-shaped types (`Request`, `Response`, `Route`, `Context`) live in `mcpr-core::proxy::pipeline::values`.

### `Request`

```rust
enum Request {
    Mcp(McpRequest),     // JSON-RPC, typed message, body owned.
    OAuth(OAuthRequest), // Discovery / token / callback.
    Raw(RawRequest),     // Everything else — forwarded unchanged.
}

struct McpRequest {
    transport:    McpTransport,      // StreamableHttpPost | StreamableHttpGet | SseLegacyGet
    envelope:     JsonRpcEnvelope,   // Shallow parse — see below.
    kind:         ClientKind,        // Classification, computed at intake.
    headers:      HeaderMap,
    session_hint: Option<SessionId>,
}
```

### `JsonRpcEnvelope` and `ClientKind` / `ServerKind`

Intake parses only the envelope: validate `"jsonrpc": "2.0"`, split out `id`, `method`, `params`, `result`, `error`. Params and results remain `Box<RawValue>` until a middleware opts in to a typed view:

```rust
struct JsonRpcEnvelope {
    id:     Option<JsonRpcId>,
    method: Option<String>,
    params: Option<Box<RawValue>>,
    result: Option<Box<RawValue>>,
    error:  Option<JsonRpcError>,
}

impl JsonRpcEnvelope {
    fn params_as<T: DeserializeOwned>(&self) -> Option<T>;
    fn result_as<T: DeserializeOwned>(&self) -> Option<T>;
}
```

The envelope pairs with a classification enum — `ClientKind` on the request side, `ServerKind` on the response side — that names the message kind without carrying payload bytes. Each method enum has a trailing `Unknown(String)` so non-spec methods forward unchanged.

`ClientMethod` covers the MCP 2025-11-25 client→server methods: `Ping`, `Lifecycle(Initialize)`, `Tools(List|Call)`, `Resources(List|TemplatesList|Read|Subscribe|Unsubscribe)`, `Prompts(List|Get)`, `Completion(Complete)`, `Logging(SetLevel)`, `Tasks(List|Get|Result|Cancel)`. `ServerMethod` covers the reverse direction (`Sampling`, `Elicitation`, `Roots`, `Tasks`, `Ping`). `ClientNotifMethod` and `ServerNotifMethod` list the notifications for each direction.

### `Route`

```rust
enum Route {
    McpStreamableHttp { upstream, method, buffer_policy },
    McpSseLegacy      { upstream },
    Oauth             { upstream, rewrite: UrlMap },
    Raw               { upstream },
}

enum BufferPolicy {
    Streamed,
    Buffered { max: usize },
}
```

The router's buffer-policy table picks `Buffered` for seven methods whose responses the pipeline inspects: `Initialize`, `Tools::List`, `Tools::Call`, `Resources::List`, `Resources::TemplatesList`, `Resources::Read`, `Prompts::List`. Every other method streams.

### `Response`

```rust
enum Response {
    McpBuffered  { envelope: Envelope, message: McpMessage, status, headers },
    McpStreamed  { envelope: Envelope, body: Body, status, headers },
    OauthJson    { doc: JsonDoc, status, headers },
    Raw          { body: Body, status, headers },
    Upstream502  { reason: String },
}

enum Envelope { Json, Sse }
```

A buffered MCP response carries exactly one `McpMessage` (envelope + `ServerKind`). Framing is a field on the response, not a code path.

### `Context`

```rust
struct Context {
    intake:  Intake,   // Immutable after intake.
    working: Working,  // Mutated by middlewares.
}

struct Intake {
    start:         Instant,
    proxy:         Arc<ProxyState>,
    http_method:   Method,
    path:          String,
    request_size:  usize,
}

struct Working {
    session:        Option<SessionInfo>,
    client:         Option<ClientInfo>,
    request_method: Option<ClientMethod>,
    request_tool:   Option<String>,
    response_size:  Option<u64>,
    tags:           TagSet,
    timings:        StageTimings,
}
```

## Middleware

Middleware is a plain async trait, not `tower::Layer`. The driver owns an ordered `Vec<Box<dyn …>>` per side and calls each one in sequence.

```rust
#[async_trait]
trait RequestMiddleware: Send + Sync {
    fn name(&self) -> &'static str;
    async fn on_request(&self, req: Request, cx: &mut Context) -> Flow;
}

enum Flow {
    Continue(Request),
    ShortCircuit(Response),
}

#[async_trait]
trait ResponseMiddleware: Send + Sync {
    fn name(&self) -> &'static str;
    async fn on_response(&self, resp: Response, cx: &mut Context) -> Response;
}
```

`build_default_pipeline(rewrite_config)` constructs the chain at startup and logs every registration at `info!`.

### Request chain

1. **`SessionDeleteMiddleware`** — on `DELETE` + `mcp-session-id`, removes the session from the store and emits `SessionEnd` before the transport forwards the DELETE upstream.
2. **`SessionTouchMiddleware`** — on MCP requests with a session hint, touches the session store, stashes the originating `ClientMethod` on `Working` for response middlewares, and flips state to `Active` on `notifications/initialized`.
3. **`ClientInfoInjectMiddleware`** — on `initialize` requests, deserializes `clientInfo` via `params_as::<Value>()` and stashes it on `Working.client`.

### Response chain

1. **`SchemaIngestMiddleware`** — on buffered `Result` responses whose originating method is a list method, spawns fire-and-forget schema ingest. Reads the raw result before any rewrite touches it; merge/hash/store runs off the hot path.
2. **`SchemaStaleMiddleware`** — on server notifications (`tools/list_changed`, `resources/list_changed`, `prompts/list_changed`), marks the corresponding method stale in the schema manager.
3. **`CspRewriteMiddleware`** — on buffered responses to `tools/list`, `tools/call`, `resources/read`, scans the result bytes for CSP markers (`connect_domains`, `openai/widgetCSP`, …). On a hit, deserializes the result, merges domains against the live `ArcSwap<RewriteConfig>`, and re-writes it back into the envelope. `mcpr.toml` reloads swap the inner `Arc` without restart.
4. **`SessionRecordMiddleware`** — on buffered `initialize` responses with status < 400, reads `mcp-session-id` from upstream headers, creates the session, attaches the `clientInfo` stashed by `ClientInfoInject`, and emits `SessionStart` with a normalized platform label.
5. **`HealthTrackMiddleware`** — on buffered or streamed `initialize` responses with status < 400, flips the shared `ProxyHealth` flag to `Connected`.
6. **`UrlMapMiddleware`** — on `OauthJson` or `Raw` JSON passthrough responses, substitutes the upstream host with the proxy host. MCP responses aren't touched: by the time `UrlMap` runs they're still `McpBuffered`, and `EnvelopeSeal` converts them to `Raw` afterwards.
7. **`EnvelopeSealMiddleware`** — on buffered responses, serializes the `McpMessage` once, wraps as SSE if `Envelope::Sse`, writes the body size + `rewritten`/`sse` tags to `Working`, and returns `Response::Raw` with the right `Content-Type`.

Order matters: ingest reads the raw upstream result before rewrite; seal runs last so content-inspecting middlewares operate on `McpBuffered`.

## Transport

`ProxyTransport::dispatch(req, route, cx)` wraps `forwarding::forward_request` and maps reqwest failures to `Response::Upstream502 { reason }`. Buffered responses run through `buffer_and_parse`, which caps the body size, unwraps SSE framing if present, parses the JSON-RPC envelope, classifies the server kind, and returns `Response::McpBuffered`. Streaming paths return `Response::McpStreamed` with the body as a `bytes_stream`.

`read_body_capped` returns a typed `ReadBodyError` that transport maps to `Upstream502`. Streaming reqwest calls keep a per-request timeout because `tower_http::timeout::TimeoutLayer` cancels at response start and can't see mid-stream stalls.

## Emit

`proxy::emit::emit(cx, resp)` is the single `RequestEvent` construction site. It runs once per request, after the response chain, inside `handle_request`. It reads accumulated state off `Context` + `Response`:

- `session_id` comes from `working.session` when a middleware touched the store, or from the response `mcp-session-id` header for `initialize` (where `SessionRecord` creates the session after the fact).
- `mcp_method` comes from `working.request_method`, with `"SSE"` literally on GET + SSE (legacy parity).
- `client_name` / `client_version` come from `working.client` first, then from the session record.
- `note` is the middleware-populated `working.tags` joined with `+`, plus shape-derived tags (`upstream error` on 502, `sse` on streamed SSE).
- `stage_timings` is `Some(StageTimings)` when any stage wrote into it, else `None`.

The event goes to `state.event_bus.emit(ProxyEvent::Request(...))`. Sinks — stderr, SQLite, cloud — are registered outside the pipeline via `EventManager`.

## Subsystems

The pipeline calls four subsystems through their existing APIs. None of them are owned by the pipeline.

| Subsystem | Location | Called from |
|---|---|---|
| `MemorySessionStore` | `protocol::session` | `SessionDelete`, `SessionTouch`, `SessionRecord` |
| `SchemaManager` | `protocol::schema_manager` | `SchemaIngest`, `SchemaStale` |
| `ProxyHealth` | `proxy::health` | `HealthTrack` |
| `EventBus` | `event` | `SessionDelete`, `SessionRecord`, `SchemaIngest`, `emit` |

`CspRewriteMiddleware` and `UrlMapMiddleware` share the `ArcSwap<RewriteConfig>` handle with `ProxyState`. Config reloads call `.store(Arc::new(new))` — the middlewares pick up the new rules on the next `.load()`.

## Error path

Upstream failures travel the response chain like any other response. `Response::Upstream502 { reason }` is produced by the transport (or a short-circuiting middleware). `HealthTrackMiddleware` sees the 502 and skips the success path. `EnvelopeSealMiddleware` ignores it. `emit` tags the event `upstream error` and sets `error_msg = reason`. `IntoResponse` renders it as a 502 body `"Upstream error: {reason}"`.

## Axum integration

| Concern | Owner | Notes |
|---|---|---|
| HTTP server + connection handling | axum | One fallback handler. No routes by path — paths are user-configurable, classification is in intake. |
| `Request` construction | intake + axum parts | `intake::from_axum_parts(method, headers, uri, body) -> Request`. |
| Edge layers | tower / tower-http | `DefaultBodyLimit`, `ConcurrencyLimitLayer`, `TimeoutLayer`, `TraceLayer`, `CorsLayer`. |
| Streaming bodies | axum | `Body::from_stream(reqwest::Response::bytes_stream(_))`. |
| `Response` → HTTP | this crate | `impl IntoResponse for Response` — variant dispatch; reuses `forwarding::build_response` for the legacy upstream-header allowlist. |
| Request / response chain | this crate | `RequestMiddleware` / `ResponseMiddleware` traits. |
| Router (upstream selection) | this crate | Pure `fn route(&Request, &Context) -> Route`. |
| Transport (upstream call) | this crate, uses `reqwest` | Buffer vs stream from `Route`, session header forwarding, typed `Response` construction. |

**Rule of thumb:** if the feature is about HTTP mechanics (how bytes move on the wire), reach for axum / tower. If it is about MCP semantics (what the bytes mean), it lives in this pipeline.

## Example: one `tools/list` request

1. Client POSTs `{"jsonrpc":"2.0","id":1,"method":"tools/list"}` to the proxy.
2. Axum body limit + concurrency limit + timeout layers accept the request.
3. `handle_request` calls `from_axum_parts` → `Request::Mcp(McpRequest { transport: StreamableHttpPost, kind: ClientKind::Request(ClientMethod::Tools(ToolsMethod::List)), … })`.
4. Request chain: `SessionDelete` no-ops (not DELETE). `SessionTouch` stashes `ClientMethod::Tools(ToolsMethod::List)` on `Working.request_method`. `ClientInfoInject` no-ops (not initialize).
5. Router returns `Route::McpStreamableHttp { buffer_policy: Buffered { max: 1 MiB }, … }`.
6. Transport POSTs to the upstream. Response is 200 `application/json` `{"jsonrpc":"2.0","id":1,"result":{"tools":[...]}}`. `buffer_and_parse` returns `Response::McpBuffered`.
7. Response chain: `SchemaIngest` spawns ingest on the raw `tools` list. `SchemaStale` no-ops. `CspRewrite` checks markers; if widget CSP is declared, rewrites domains against the live `RewriteConfig`. `SessionRecord` no-ops (wrong method). `HealthTrack` no-ops. `UrlMap` no-ops (still `McpBuffered`). `EnvelopeSeal` serializes the envelope, pushes `rewritten` onto `Working.tags`, writes `response_size`, returns `Response::Raw`.
8. `emit` builds a `RequestEvent` (`mcp_method = "tools/list"`, `note = "rewritten"`, timings + size populated) and writes it to the event bus.
9. `IntoResponse` returns the axum response with the rewritten bytes and `application/json` Content-Type.
