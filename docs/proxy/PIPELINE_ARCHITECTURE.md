# Proxy Pipeline Architecture

Design document for the mcpr proxy request pipeline. This is a target architecture — it describes the shape we want, not the shape we have today. A "Gaps vs current code" section at the end lists what has to move.

## Scope

mcpr is a **routing + observability proxy** for MCP traffic. It forwards JSON-RPC between clients and upstream MCP servers and extracts signal from that traffic (sessions, health, schema discovery, request events). It is not a general web server — we do not serve static assets, render HTML, or host local content. If a request does not belong to the MCP control plane, it is forwarded raw or rejected.

## Goals

- **No path assumptions.** Mount paths (MCP endpoint, OAuth discovery, etc.) are user-configurable. The pipeline classifies by request *content*, never by hard-coded URL paths.
- **Types carry invariants.** If a value is `Request::Mcp`, it *is* JSON-RPC. No `Option<ParsedBody>` downstream.
- **Pattern-match, don't branch on booleans.** Each middleware matches the variants it cares about; it is a no-op for everything else.
- **Trait-driven middleware.** Request and response stages are values implementing a trait, registered in an ordered chain. Adding a stage is a registration, not a handler edit.
- **Ownership moves forward.** Bodies, parsed docs, and route decisions are moved between stages. Clones are reserved for `Bytes` / `Arc` handles at system boundaries.
- **Lean on axum / tower at the HTTP boundary.** Use axum / tower for plumbing that already exists — `Router`, `FromRequest`, `IntoResponse`, `Body`, timeout, concurrency limit, tracing, request-id. Do not reinvent routing or body streaming.
- **Own the domain middleware.** Our request/response middleware is a small, focused trait system — not `tower::Layer`. Tower is built around opaque generic `Service<Request>` types; our middleware needs variant-aware pattern matching, short-circuit, shared typed `Context`, and a typed `Response` return. Wrapping all of that into tower's type machinery adds boilerplate for no benefit. A plain async trait + an ordered list is simpler, easier to test, and easier to extend with advanced features (conditional chains, per-upstream overrides, etc.).
- **Streamable HTTP is primary; SSE is legacy.** The MCP transport we design around is Streamable HTTP (POST, possibly chunked response). SSE (`GET` with `text/event-stream`) is supported but treated as a secondary branch, not a first-class code path.
- **Observability is a first-class output.** Every request produces a `RequestEvent`. Every response updates health and, where applicable, schema state. Emission is not a side effect of a handler — it is a pipeline stage.

## Non-goals

- Not a rewrite of the protocol layer (`mcpr-core::protocol`).
- Not a change to `mcpr.toml` config shape (router reads config; config layout is separate work).
- Not a session-store redesign.
- **Not a static-asset or widget server.** The upcoming MCP spec revision does not permit the proxy to serve local static content for widgets. We remove the local serving path (widget HTML, widget asset, widget listing endpoints) rather than carry dead code. **CSP rewriting stays** — it is first-class configuration and must survive. Upstream MCP responses carry widget CSP directives that reference upstream hosts; the proxy rewrites them so widgets loaded via the proxy can still reach the resources they need. That work moves into a response middleware (see `CspRewriteMiddleware` below), backed by an `ArcSwap<RewriteConfig>` so `mcpr.toml` reloads apply without a restart.

---

## Locked decisions

Design choices already committed — no longer open for discussion in this doc.

1. **`#[async_trait]`** for middleware traits. Simple, proven; we `Box<dyn ...>` anyway so the allocation cost is a non-issue.
2. **Static middleware chain, built at startup.** An ordered `Vec<Box<dyn RequestMiddleware>>` constructed once from config. Reordering is a code/config change, not runtime. The constructor optionally logs each registration (`info!("middleware registered: {name}")`) for operator visibility.
3. **Two-stage parse.** Intake parses only the JSON-RPC envelope — cheap. Method identity is classified at intake (`ClientMethod`, `ServerMethod`). Typed params (`CallToolParams`, `InitializeResult`, …) are deserialized lazily, only by middlewares that need them. See the "MCP message taxonomy" section.
4. **No batching.** MCP 2025-11-25 does not support JSON-RPC batches. One HTTP POST = exactly one JSON-RPC message. This removes an entire class of complexity from the pipeline.

---

## Principles

1. **One sum type per boundary.** `Request`, `Route`, `Response`. Each boundary is a single enum; stages pattern-match.
2. **Pure router, I/O in transport.** The router is a pure function of `(Request, Config)`. Only the transport layer speaks to the network.
3. **Middleware is variant-aware.** A middleware declares which variants it applies to; the chain driver skips non-matching variants. No `if let Some(x) = ...` boilerplate in handlers.
4. **Envelope is data, not control flow.** Whether an MCP response is wrapped in SSE framing is a field on the response value, resolved once at the final `EnvelopeSeal` stage. Upstream handlers never re-parse / re-wrap.
5. **One parse, one serialize.** A buffered MCP response is parsed into a `JsonDoc` once; middlewares mutate in place; serialization happens once on the way out.

---

## Layers

```
 HTTP (axum)
   │
   ▼                                One axum fallback handler receives every
 ┌──────────────────────────┐        request; classification is content-based,
 │ Intake (content-first)   │        not path-based. Order:
 │  raw request → Request   │          1) Try parse as MCP JSON-RPC
 └──────────────────────────┘          2) Else try match as OAuth / discovery
                                       3) Else Raw (forwarded) or rejected
   │  Request owned, body owned
   ▼
 ┌──────────────────────────┐
 │ Request middleware chain │      Ordered Vec<Box<dyn RequestMiddleware>>,
 │  trait RequestMiddleware │      built at startup. Each middleware pattern-
 └──────────────────────────┘      matches on Request variants. Short-circuits
                                  produce a Response early.
   │  Request (maybe short-circuited to Response)
   ▼
 ┌──────────────────────────┐
 │ Router (pure)            │      fn route(&Request, &Config) -> Route
 └──────────────────────────┘      No I/O. Testable in isolation.
   │  (Request, Route)
   ▼
 ┌──────────────────────────┐
 │ Transport                │      The only layer touching the network.
 │  upstream dispatch       │      Timeouts, semaphore, body collection.
 └──────────────────────────┘      Errors become Response::Upstream502.
   │  Response
   ▼
 ┌──────────────────────────┐
 │ Response middleware chain│      Same trait pattern. Mutates JsonDoc,
 │  trait ResponseMiddleware│      records session state, ingests schema.
 └──────────────────────────┘      Ends with EnvelopeSeal.
   │  Response (sealed)
   ▼
 ┌──────────────────────────┐
 │ Emit                     │      RequestEvent → event bus (TUI, cloud).
 └──────────────────────────┘
   │
   ▼
 IntoResponse → axum::Response
```

---

## Intake classification

**URL paths are user-configurable.** The MCP mount path, OAuth discovery paths, and everything else are set in `mcpr.toml`. The intake layer therefore does **not** branch on specific path strings. It classifies by request content, in this order:

1. **Try MCP first.** Attempt to parse the body as JSON-RPC 2.0. If it parses into one or more `JsonRpcMessage`s, the request is `Request::Mcp`. Covers `POST` (streamable HTTP) and `GET + Accept: text/event-stream` (legacy SSE — a GET with no body but an MCP-shaped `Accept` is still an MCP request).
2. **Else try OAuth.** If content / headers match a known OAuth or discovery shape (well-known metadata, token endpoints, authorization callback), the request is `Request::OAuth`. Detection is content-based (payload shape, `Content-Type`, discovery response keys), not path-based.
3. **Else it is out of scope.** Everything remaining is either `Request::Raw` (forwarded unchanged, no inspection) or rejected by the top-level axum layer — we are a routing + observability proxy, not a general web server.

Axum's role here is limited: it is the fallback `Service` that hands us method / headers / body. We do the classification. When we do reject, we return an axum response directly — no custom machinery.

## MCP message taxonomy (spec v2025-11-25)

Source of truth: [MCP schema.ts @ 2025-11-25](https://github.com/modelcontextprotocol/modelcontextprotocol/blob/main/schema/2025-11-25/schema.ts). `LATEST_PROTOCOL_VERSION = "2025-11-25"`. `JSONRPC_VERSION = "2.0"`.

A proxy sits between a **Client** (host, e.g. an IDE) and a **Server** (tool provider). Both directions carry JSON-RPC payloads — the proxy must model both. A single streamable-HTTP connection is bidirectional: the client's POST body carries one family of messages, and the response body (which may be a chunked stream) carries the other.

### The four message kinds (JSON-RPC 2.0, as used by MCP)

For each direction, a payload is one of:

- **Request** — `{ jsonrpc, id, method, params? }` — expects a response.
- **Notification** — `{ jsonrpc, method, params? }` — no id, no response.
- **Response (result)** — `{ jsonrpc, id, result }` — reply to a prior request from the *other* side.
- **Response (error)** — `{ jsonrpc, id, error }` — failure reply to a prior request.

**No batching.** JSON-RPC 2.0 allows arrays of messages as an optional feature; MCP 2025-11-25 does not adopt it. The base-protocol spec enumerates only the three kinds above. One HTTP POST = exactly one JSON-RPC message.

### Direction → concrete methods

**Client → Server**

| Kind | Method | Purpose |
|---|---|---|
| Request | `initialize` | capability negotiation |
| Request | `ping` | liveness |
| Request | `tools/list` | enumerate tools |
| Request | `tools/call` | invoke a tool |
| Request | `resources/list` | list resources |
| Request | `resources/templates/list` | list resource templates |
| Request | `resources/read` | read a resource URI |
| Request | `resources/subscribe` | subscribe to resource updates |
| Request | `resources/unsubscribe` | cancel subscription |
| Request | `prompts/list` | list prompt templates |
| Request | `prompts/get` | get a prompt |
| Request | `completion/complete` | arg autocomplete |
| Request | `logging/setLevel` | set server log level |
| Request | `tasks/list` / `tasks/get` / `tasks/result` / `tasks/cancel` | task management |
| Notification | `notifications/initialized` | init handshake complete |
| Notification | `notifications/cancelled` | cancel a prior client→server request |
| Notification | `notifications/progress` | progress on a prior client→server request |
| Notification | `notifications/roots/list_changed` | client's roots changed |

**Server → Client** (appear inside response streams, not in the POST body)

| Kind | Method | Purpose |
|---|---|---|
| Request | `ping` | liveness |
| Request | `roots/list` | ask client for roots |
| Request | `sampling/createMessage` | ask client to run an LLM sample |
| Request | `elicitation/create` | ask client to collect input from the user |
| Request | `tasks/list` / `tasks/get` / `tasks/result` / `tasks/cancel` | task management |
| Notification | `notifications/cancelled` | cancel a prior server→client request |
| Notification | `notifications/progress` | progress on a prior server→client request |
| Notification | `notifications/message` | structured log from server |
| Notification | `notifications/tools/list_changed` | server's tool list changed |
| Notification | `notifications/resources/list_changed` | server's resource list changed |
| Notification | `notifications/resources/updated` | a subscribed resource changed |
| Notification | `notifications/prompts/list_changed` | server's prompt list changed |
| Notification | `notifications/elicitation/complete` | out-of-band elicitation finished |
| Notification | `notifications/tasks/status` | task status changed |

Error codes the proxy forwards unchanged: `-32700 PARSE_ERROR`, `-32600 INVALID_REQUEST`, `-32601 METHOD_NOT_FOUND`, `-32602 INVALID_PARAMS`, `-32603 INTERNAL_ERROR`, `-32042 URL_ELICITATION_REQUIRED` (spec-specific).

---

## Types

### `Request`

Top-level sum type produced by Intake. Owns its body.

```
enum Request {
    Mcp(McpRequest),        // JSON-RPC, typed message(s), body owned
    OAuth(OAuthRequest),    // discovery / token / callback — content-matched
    Raw(RawRequest),        // everything else — forwarded unchanged
}
```

### `McpRequest`

An MCP HTTP request from the client. The body carries exactly one JSON-RPC message (MCP does not support batches). The message is one of four kinds — all must be representable, because the client may POST a *response* to a prior server-initiated request.

```
struct McpRequest {
    transport:    McpTransport,
    envelope:     JsonRpcEnvelope,   // shallow — see below
    kind:         ClientKind,        // classification, computed at intake
    headers:      HeaderMap,         // Authorization, Accept, mcp-session-id, last-event-id
    session_hint: Option<SessionId>,
}

enum McpTransport {
    StreamableHttpPost,   // primary — POST body carries client→server messages
    StreamableHttpGet,    // optional GET to open a server-push stream
    SseLegacyGet,         // legacy HTTP+SSE (demoted)
}
```

### Two-stage parse — envelope first, types on demand

Intake parses only the JSON-RPC envelope. This is cheap: validate `"jsonrpc": "2.0"`, split out `id`, `method`, `params`, `result`, `error`. Params and results remain as `RawValue` — unparsed bytes — until a middleware opts in to a typed view.

```
struct JsonRpcEnvelope {
    id:     Option<JsonRpcId>,
    method: Option<String>,           // None for responses
    params: Option<Box<RawValue>>,    // unparsed; opt-in typed views
    result: Option<Box<RawValue>>,    // for responses
    error:  Option<JsonRpcError>,
}

impl JsonRpcEnvelope {
    fn params_as<T: DeserializeOwned>(&self) -> Option<T>;
    fn result_as<T: DeserializeOwned>(&self) -> Option<T>;
}
```

Middlewares that only need to know *what kind of message this is* (session touch, health track) match on `ClientKind` and never touch `params`. Middlewares that need typed access (`ClientInfoInject` reading `InitializeParams`, `SchemaIngest` reading `ListToolsResult`) call `params_as::<T>()` / `result_as::<T>()` once. No per-method deserializer runs for requests nobody inspects.

### Classification enums (no payload)

```
// What kind of message is the client sending? Computed at intake by looking
// at method + id + result/error presence. Carries no deserialized params.
enum ClientKind {
    Request(ClientMethod),           // method + id → needs response
    Notification(ClientNotifMethod), // method, no id → no response
    Result,                          // id + result → reply to prior server request
    Error,                           // id + error  → error reply to prior server request
}

enum ClientMethod {
    Ping,
    Lifecycle(LifecycleMethod),
    Tools(ToolsMethod),
    Resources(ResourcesMethod),
    Prompts(PromptsMethod),
    Completion(CompletionMethod),
    Logging(LoggingMethod),
    Tasks(TasksMethod),
}

enum LifecycleMethod  { Initialize }
enum ToolsMethod      { List, Call }
enum ResourcesMethod  { List, TemplatesList, Read, Subscribe, Unsubscribe }
enum PromptsMethod    { List, Get }
enum CompletionMethod { Complete }
enum LoggingMethod    { SetLevel }
enum TasksMethod      { List, Get, Result, Cancel }

enum ClientNotifMethod {
    Initialized,
    Cancelled,
    Progress,
    RootsListChanged,
    TaskStatus,
}
```

Method identity is a cheap enum — one string-match per request. Grouping by feature area means middlewares pattern-match at the right level of granularity (`ClientMethod::Tools(_)` vs the specific variant).

### The reverse direction — `ServerKind`

Server-initiated messages appear in the **response** body of a streamable-HTTP POST (or a GET stream, or legacy SSE). Same two-stage-parse discipline: shallow envelope + classification + lazy typed views.

```
enum ServerKind {
    Request(ServerMethod),              // server→client request
    Notification(ServerNotifMethod),
    Result,                             // reply to a prior client→server request
    Error,
}

enum ServerMethod {
    Ping,
    Sampling,                           // sampling/createMessage
    Elicitation,                        // elicitation/create
    Roots,                              // roots/list
    Tasks(TasksMethod),
}

enum ServerNotifMethod {
    Cancelled,
    Progress,
    LogMessage,                         // notifications/message
    ResourcesListChanged,
    ResourceUpdated,
    ToolsListChanged,
    PromptsListChanged,
    ElicitationComplete,
    TaskStatus,
}
```

`ServerKind::Result` / `ServerKind::Error` do not carry the method they are responding to. Correlating a response id → originating method requires pending-request tracking, which we treat as a separate subsystem (out of scope for v1 — middlewares that need typed results today use duck-typed `result_as::<T>()` and match on `Some(value)`).

### A typed MCP message pair

```
struct McpMessage {
    envelope: JsonRpcEnvelope,
    kind:     ClientKind,    // or ServerKind in the reverse direction
}
```

Used inside `McpRequest` (client direction) and inside `Response::McpBuffered` (server direction, see below).

### Why this shape

- **Fast intake.** One envelope parse + one string-to-enum match per message. No deserialization of params the pipeline will never read.
- **Pattern matching where middlewares want it.** `ClientMethod::Tools(ToolsMethod::Call)` is a cheap match — no allocator, no serde.
- **Typed access where middlewares need it.** One line: `msg.envelope.params_as::<CallToolParams>()`.
- **Passthrough is byte-cheap.** If a middleware doesn't mutate, the original bytes forward unchanged — no re-serialization.
- **Types carry invariants.** `Request::Mcp` means MCP. `ClientKind::Notification` means no response expected. No `Option<ParsedBody>` downstream.

### `Route`

Output of the router. Declarative; no I/O.

```
enum Route {
    McpStreamableHttp { upstream, method, buffer_policy }, // primary path
    McpSseLegacy      { upstream },                        // GET + event-stream
    Oauth             { upstream, rewrite: UrlMap },
    Raw               { upstream },
}

enum BufferPolicy {
    Streamed,                // forward bytes, don't inspect
    Buffered { max: usize }, // collect, parse, allow mutation
}
```

**Why a policy field instead of a method predicate.** Whether `tools/call` gets buffered is a routing decision, not an intrinsic property of the method. The routing table owns this.

### `Response`

Sum type produced by Transport (or a short-circuited middleware).

```
enum Response {
    McpBuffered  { envelope: Envelope, message: McpMessage, status: StatusCode },
    McpStreamed  { envelope: Envelope, body: Body, status: StatusCode },
    OauthJson    { doc: JsonDoc, status: StatusCode },
    Raw          { body: Body, status: StatusCode, headers: HeaderMap },
    Upstream502  { reason: String },
}

enum Envelope { Json, Sse }
```

A buffered MCP response carries exactly one `McpMessage` (shallow envelope + `ServerKind` classification). No `Batch` variant — MCP does not batch.

**Why `Envelope` lives on the response, not on a branch.** Today the buffered handler unwraps SSE, parses, mutates, reserializes, rewraps — all inline. Making envelope a field lets middlewares touch the typed `message` without caring about framing; a single `EnvelopeSeal` stage applies the wrap at the end.

**Why shallow envelope instead of a raw `JsonDoc` or a fully-typed payload.** Response middlewares pattern-match on `ServerKind::Result` / `ServerKind::Notification(ServerNotifMethod::ResourcesListChanged)` — cheap match. When they need typed content (`SchemaIngest` deserializing `ListToolsResult`), they call `message.envelope.result_as::<ListToolsResult>()`. One parse, one serialize, typed where it matters.

### `Context`

Carried by reference through the chain. Split into two halves so that immutability after intake is visible in the type system.

```
struct Context {
    intake: Intake,          // immutable after Intake builds it
    working: Working,        // mutated by middlewares
}

struct Intake {
    start: Instant,
    proxy: Arc<ProxyRuntime>,
    http_method: Method,
    path: String,
    request_size: usize,
}

struct Working {
    session: Option<SessionRecord>,
    client:  Option<ClientInfo>,
    tags:    TagSet,
    timings: StageTimings,
}
```

---

## Middleware

Our middleware is a plain async trait, not a `tower::Layer`. The driver is a short, explicit loop that owns an ordered `Vec<Box<dyn ...>>` and calls each middleware in sequence. That is the whole machinery — no service combinators, no type-level composition.

### Request middleware

```
#[async_trait]
trait RequestMiddleware: Send + Sync {
    async fn on_request(&self, req: Request, cx: &mut Context) -> Flow;
}

enum Flow {
    Continue(Request),
    ShortCircuit(Response),
}
```

Each middleware matches on the variants it cares about and returns `Continue(req)` unchanged otherwise. Short-circuits skip the router and transport (e.g. `SessionDelete` → 202 after session tear-down).

### Response middleware

```
#[async_trait]
trait ResponseMiddleware: Send + Sync {
    async fn on_response(&self, resp: Response, cx: &mut Context) -> Response;
}
```

Mirrors the request side. No `Flow` — responses always pass through; a middleware that wants to replace the response returns a different variant (e.g. `Upstream502 → Raw(error page)`).

### Driver

```
struct Pipeline {
    request_chain:  Vec<Box<dyn RequestMiddleware>>,
    response_chain: Vec<Box<dyn ResponseMiddleware>>,
    router:    Router,
    transport: Transport,
}

impl Pipeline {
    async fn run(&self, req: Request, cx: &mut Context) -> Response {
        let mut req = req;
        for mw in &self.request_chain {
            match mw.on_request(req, cx).await {
                Flow::Continue(r)   => req = r,
                Flow::ShortCircuit(r) => return self.run_response(r, cx).await,
            }
        }
        let route = self.router.route(&req);
        let resp  = self.transport.dispatch(req, route, cx).await;
        self.run_response(resp, cx).await
    }

    async fn run_response(&self, mut resp: Response, cx: &mut Context) -> Response {
        for mw in &self.response_chain {
            resp = mw.on_response(resp, cx).await;
        }
        resp
    }
}
```

That is the whole engine. It is trivially testable, trivially traceable, and does not depend on tower.

### Why not `tower::Layer`

- Tower `Service<Req>` is parameterised over a single request/response type pair. Our `Request` / `Response` are sum types, and several middlewares want to short-circuit with a `Response` — expressible in tower only via `Result<Response, Response>` and custom errors. Noisy.
- Tower middleware composition happens at type level via `ServiceBuilder`. Our chain composition is at runtime (config-driven registration). Tower's advantage disappears.
- We share a typed `Context` across the chain. Threading it through `Service::call` with tower is possible, but means either extension maps (dynamic typing, back where we started) or elaborate associated types.
- Advanced features we want — conditional middlewares per upstream, hot-reload ordering, per-session chains — are one line of configuration in our trait, and non-trivial in tower.

Tower still owns the HTTP boundary (rate limit, timeout, request id, trace). Inside the pipeline, we use our own trait.

---

## Axum mapping

Clear split: **axum / tower owns the HTTP edge; our trait owns the domain pipeline.**

| Concern | Who | Notes |
|---|---|---|
| HTTP server + connection handling | axum | Single fallback handler. We do **not** split routes by path — paths are user-configurable; classification is in Intake, content-based. |
| `Request` construction | axum + our code | A `FromRequest` impl reads method / headers / body from axum and returns our `Request` enum. |
| Transport-level concerns | tower / tower-http | `TimeoutLayer`, `ConcurrencyLimitLayer`, `TraceLayer`, request-id, compression. Wired as axum layers once, globally. |
| Streaming bodies | axum | `axum::body::Body::from_stream(reqwest::Response::bytes_stream(_))`. |
| `Response` → HTTP | our code | `impl IntoResponse for Response` — variant dispatch to axum responses. SSE re-wrap is a local helper. |
| Request / response middleware chain | **our trait** | `RequestMiddleware` / `ResponseMiddleware`. Sequential, variant-aware, short-circuitable. Not tower. |
| Router (upstream selection) | **our code** | Pure `fn route(&Request, &Config) -> Route`. Not axum's routing. |
| Transport (upstream call) | **our code**, uses `reqwest` | Domain-specific: buffer vs stream decision, session header capture, typed `Response` construction. |
| Observability | **our code** | `Emit` stage produces `RequestEvent` for the TUI and cloud. Separate from `TraceLayer` spans. |

**Rule of thumb:** if the feature is about *HTTP mechanics* (how bytes move on the wire), reach for axum / tower. If it is about *MCP semantics* (what the bytes mean), it lives in our pipeline.

---

## The MCP transport split

MCP has moved to **Streamable HTTP** as its primary transport. A single `POST /mcp` may return either:

- a single JSON body (`application/json`), or
- a chunked stream of JSON-RPC messages (`text/event-stream` framing, but logically a streamable response, not the legacy two-channel SSE pattern).

We model the primary path as `Route::McpStreamableHttp { buffer_policy }`. The transport layer collects or streams based on `buffer_policy`, not on content-type sniffing.

Legacy `GET /mcp` with `Accept: text/event-stream` — the old HTTP+SSE transport — remains supported as `Route::McpSseLegacy`. It is a pure passthrough: no buffering, no inspection, no session side-effects beyond touch. We keep it working; we do not invest in it.

Response middlewares that inspect the body (schema ingest) only fire on `McpBuffered`. A streamable response with `buffer_policy = Streamed` skips them entirely — by pattern-match, not by a guard flag.

---

## Existing subsystems

The pipeline calls several subsystems that already exist in the crate. None of them are rebuilt as part of this architecture — middlewares consume them through their current APIs.

| Subsystem | Location | Role |
|---|---|---|
| `MemorySessionStore` | `protocol/session.rs` | Per-request session lookup / touch / create. Async `SessionStore` trait, in-memory `DashMap`. Session lifecycle events are emitted by the middlewares that mutate it (`SessionTouch`, `SessionRecord`, `SessionDelete`), not by the store itself. |
| `SchemaManager` | `protocol/schema_manager/` | Ingests list-method results (`tools/list`, `resources/list`, …) into a schema store. Middleware calls `ingest`; no changes to the manager. |
| `ProxyHealth` / `SharedProxyHealth` | `proxy/health.rs` | Records success / failure / upstream errors per proxy instance. Middleware calls `record`. |
| `EventBus` | `event/` | Fan-out to TUI / cloud dashboard sinks. Middlewares call `state.event_bus.emit(ProxyEvent::…)` directly. |
| `RewriteConfig` + `ArcSwap<RewriteConfig>` | `proxy/rewrite.rs` | Hot-reloadable CSP rewrite rules. `CspRewriteMiddleware` holds the `ArcSwap` handle. |

**Deferred — not part of this refactor:**

- **Session subsystem tidy-up** (rename to `SessionManager`, collapse state enum, centralize event emission, add idle reaper). Separate track; the async `MemorySessionStore` API stays in place for v1 of the new pipeline.
- **`ServerPushBroker` and server-push middlewares.** No subscribe/publish plumbing today; the legacy SSE path stays a byte passthrough. Added when server→client push observability becomes a required feature.

## Registered chains (baseline)

Each middleware is a thin translator. State lives in the existing subsystems listed above.

**Request chain, in order:**

1. `SessionDeleteMiddleware` — matches `Request::Mcp(_)` + HTTP `DELETE`. Forwards DELETE upstream, calls `state.sessions.remove(id).await`, emits `ProxyEvent::SessionEnd`, short-circuits 204.
2. `SessionTouchMiddleware` — matches `Request::Mcp(_)` with session id present. Calls `state.sessions.touch(id).await`, stashes the looked-up `SessionInfo` on `cx.working.session`.
3. `ClientInfoInjectMiddleware` — matches `ClientKind::Request(ClientMethod::Lifecycle(_))` → deserializes `InitializeParams` via `params_as::<T>()` → stores `clientInfo` in `cx.working.client`.

**Response chain, in order:**

1. `SchemaIngestMiddleware` — matches `McpBuffered` where the incoming `ClientKind` was one of `Tools::List | Resources::List | Resources::TemplatesList | Prompts::List` → deserializes the corresponding result via `result_as::<T>()` → `SchemaManager::ingest`.
2. `SessionRecordMiddleware` — matches `McpBuffered` where the incoming `ClientKind` was `Lifecycle::Initialize` and status < 400. Captures `mcp-session-id` from upstream headers, calls `state.sessions.create(id).await` + `set_client_info`, emits `ProxyEvent::SessionStart`.
3. `HealthTrackMiddleware` — matches `McpBuffered | McpStreamed | Upstream502` → `ProxyHealth::record` on the `SharedProxyHealth` held by `ProxyState`.
4. `CspRewriteMiddleware` — matches `McpBuffered` where the incoming `ClientKind` was one of `Tools::List | Tools::Call | Resources::Read` (the methods that carry widget CSP directives in their results) → cheap byte pre-scan for CSP markers, skip on miss; on hit, deserialize the result, merge domains against the live `ArcSwap<RewriteConfig>`, reserialize into the same `McpMessage`. Config reloads from `mcpr.toml` swap the inner `Arc` — no middleware restart.
5. `UrlMapMiddleware` — matches `OauthJson` → rewrites upstream URLs to proxy URLs in discovery payloads.
6. `EnvelopeSealMiddleware` — matches `McpBuffered` → serializes the `McpMessage` once, re-wraps as SSE if `Envelope::Sse`.

Order matters: ingest before rewrite (schema reads the pre-rewrite shape), rewrite before seal (seal is the single serialize point), seal before emit. The final `Emit` stage (outside the middleware chain) always runs and writes the `RequestEvent` to the event bus.

Server-push middlewares (`ServerPushSubscribe`, `ServerPushPublish`) are not registered in v1 — no `ServerPushBroker` exists yet. When push observability lands, they slot in as a request middleware (subscribe on `McpTransport::StreamableHttpGet`) and a response middleware (publish on `ServerKind::Notification(_) | Request(_)`).

---

## Error path

Upstream failures become `Response::Upstream502 { reason }`. They travel through the response chain like any other response:

- `HealthTrack` records the failure.
- `EnvelopeSeal` is a no-op.
- `IntoResponse` renders a 502 with the reason.

This replaces today's ad-hoc `emit_upstream_error` which skips middlewares and emits inline.

---

## Batch JSON-RPC

Not applicable. MCP 2025-11-25 does not adopt JSON-RPC batching. One POST = one message. No driver unroll, no middleware iteration.

---

## Gaps vs current code

What has to move to reach this design:

1. **Introduce `Request` sum type.** Today = wide `RequestContext` with optional JSON-RPC. Collapse option-ridden fields into the enum.
2. **Introduce `Response` sum type.** Handlers currently construct `axum::Response` inline; `IntoResponse` on the enum replaces that.
3. **Split `RequestContext` into `Intake` + `Working`.** Intake immutable after parse; working holds session/client/tags/timings. Prevents accidental stale reads.
4. **Extract `RequestMiddleware` / `ResponseMiddleware` traits.** `steps/*.rs` today are free functions hand-wired inside `buffered.rs` / `passthrough.rs`. Becomes a registered chain.
5. **Separate Router from Transport.** `classify_request` + handler dispatch are fused today. Router becomes pure; transport is the one I/O layer.
6. **Move `needs_response_buffering` off `McpMethod`.** Buffer policy belongs in the router table, not the protocol enum.
7. **Envelope as data.** `Response::McpBuffered { envelope, message }` + `EnvelopeSeal` replace the unwrap→mutate→rewrap pattern in `buffered.rs`.
8. **Two-stage parse.** Replace the current "parse everything up front" with a shallow `JsonRpcEnvelope` + `ClientKind` / `ServerKind` classification. Typed params deserialize lazily via `params_as::<T>()` / `result_as::<T>()`. Today's `ParsedBody` and per-method deserialize logic collapse into one small parser.
9. **Error path as a variant.** `Upstream502` flows through response middlewares; remove `emit_upstream_error`.
10. **Delete the widget static-asset path; keep CSP rewriting as a middleware.** `WidgetHtml`, `WidgetList`, `WidgetAsset` `RequestKind` variants, the `widgets` handler module, and `steps/widget.rs` overlay are removed — the proxy no longer serves local widget content. **CSP rewriting stays**: `csp.rs` (the config types and `effective_domains` merge), `rewrite.rs` (`rewrite_response`), and the `ArcSwap<RewriteConfig>` hot-reload plumbing all survive. The logic in `steps/rewrite.rs` (`has_markers` byte pre-scan + `rewrite_in_place`) moves into `CspRewriteMiddleware` on the response chain. `mcpr.toml` keeps the `[csp]` section; operators can continue to tune directives and reload without restart.
11. **Session / health / schema call sites move into middlewares.** Today session state is touched in `header_phase.rs`, `steps/session.rs`, `buffered.rs`, and `streamed.rs`. In the target, four variant-matched middlewares (`SessionTouch`, `SessionDelete`, `SessionRecord`, `ClientInfoInject`) are the only sites that know about sessions. They call the existing `MemorySessionStore` API. Same pattern for `ProxyHealth` and `SchemaManager`. A later session refactor may tighten the store API, but it is not required for this refactor.
12. **Lean on tower primitives.** Replace any hand-rolled timeout / concurrency / semaphore code with `tower::limit` + `tower_http::timeout`.
13. **SSE demoted.** Primary path is `McpStreamableHttp`; `McpSseLegacy` is a single small branch with no response middlewares attached. Server-push observability (`ServerPushBroker`) is deferred to separate work.

---

## Open questions

- **Pending-request tracker.** Correlating a `ServerKind::Result` back to the originating client `ClientMethod` needs a per-session `HashMap<JsonRpcId, ClientMethod>` that the request path writes and the response path reads. This unlocks typed response inspection (e.g. "this Result is for `Tools::List` → deserialize as `ListToolsResult`") without duck-typing. Probably a small subsystem (`PendingRequestTracker`) with two thin middlewares. Needs its own design pass.
- **Per-upstream / per-session chains.** Static list is the default. If we need per-upstream middleware selection (e.g. one upstream doesn't want schema ingest), the trait supports it; the driver needs a `chain_for(&Request)` selector. Defer until a concrete upstream demands it.
- **Typed-params caching.** `params_as::<T>()` re-runs `serde_json::from_str` each call. If the same middleware chain deserializes the same params twice (unlikely today), a `OnceCell<TypedParams>` on the envelope would cache. Measure before optimising.
