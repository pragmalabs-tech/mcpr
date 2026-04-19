# Proxy pipeline refactor plan

> **Status (2026-04-19): LANDED.** All seven steps in the implementation
> plan have been executed. The `middleware/` directory is deleted,
> `ResponseContext` is gone, `classify_request` routes to concrete
> handlers under `pipeline/handlers/`, and `rewrite_config` is
> `ArcSwap`. SSE byte-pass correctness verified via
> `benches/scripts/scenarios/{sse-compat,multi-event-sse,passthrough-binary}.sh`.
> Before/after numbers in `benches/reports/v0.4.42-post-refactor.md`.
>
> This doc is kept as historical context — the architecture described
> as "target" below is now the architecture in the code.

**Why this doc exists**: the original request pipeline buffered everything,
forced every response through a fixed 8-middleware chain, and handled SSE
wrong. This plan laid out what was broken, why it was hard to maintain, and
the concrete steps to replace it with a branch-by-shape design modeled
after Pingora / Envoy / Kong.

All file:line references are against the working tree at the time of
writing (one commit past v0.4.41). Don't trust them blindly after
refactor work lands.

---

## Part 1 — What's broken

### 1.1 SSE: buffered when it should stream

`pipeline/run.rs:123` calls `read_body_capped` on every POST /mcp
response, including ones the upstream sent as `text/event-stream`. This
means:

- **`transfer-encoding: chunked` becomes `content-length: N`** on the
  way through mcpr. The client sees a different framing than the server
  sent. Visible in `sse-compat.sh` — `ok` on content-type, `FAIL` on
  chunked preservation.
- **Multi-event SSE streams are clipped**. `proxy/sse.rs:10-22`
  (`extract_json_from_sse`) returns `Some` only when there's **exactly
  one** non-empty `data:` line. Multiple events → returns `None`, which
  the downstream middleware chain interprets as "not SSE at all" → body
  bytes pass through unchanged. That works for byte fidelity by accident,
  but means no SSE response with more than one event can have its inner
  JSON mutated. Silent protocol gap.
- **SSE metadata is lossy**. When exactly one event is present,
  `extract_json_from_sse` strips `id:`, `retry:`, `event:` lines and
  empty `data:` prefix lines. The paired `wrap_as_sse` then emits only
  `data: {payload}\n\n`. On any mutation round-trip, upstream SSE
  metadata is destroyed.
- **Memory unbounded by design**. `read_body_capped`'s cap is a defense,
  not a strategy — a 10 MB SSE stream of progress notifications will
  buffer 10 MB before sending a single byte to the client.

### 1.2 POST responses: every one goes through the decode → mutate → encode bracket

`pipeline/run.rs:137-160` runs the same 8-middleware chain for every
POST, regardless of:

- whether the body is JSON, SSE, binary, or empty
- whether the request method is one that could be mutated
  (`tools/list`, `resources/*` with widget metadata)
- whether the upstream response is a success or a 4xx/5xx error

Concrete waste per request:

- **Unconditional JSON parse** (sse.rs:33 `serde_json::from_slice`).
  Allocates a full `serde_json::Value` tree for every response, even if
  no downstream middleware will touch it.
- **Unconditional RwLock read** (rewrite_url.rs:25, plus
  upstream_url_map.rs:37). Two async locks taken per response even for
  methods that can't be rewritten.
- **Full-body UTF-8 conversion** for non-JSON responses
  (upstream_url_map.rs:40 via `String::from_utf8_lossy` + `.replace()`)
  even if the upstream URL doesn't appear in the body. This is
  especially bad because non-JSON responses could contain arbitrary
  bytes that `from_utf8_lossy` slow-paths through.
- **JSON reserialization** (before the band-aid: unconditional; after
  the current `json_mutated` fix: still triggered for any method that
  enters the rewrite middleware and actually finds something to
  mutate).

### 1.3 Three near-duplicate handlers with subtly different behavior

`pipeline/run.rs:73-336` has `mcp_post`, `mcp_sse`, and `passthrough`.
Each does roughly the same thing — forward, check error, build response,
emit event — but with different middleware sets, different error
response formats, and different code for what "session info" means.
Specifically:

- `mcp_sse` (line 232) **does stream** via `Body::from_stream` —
  contradicting the POST path. So mcpr already proves it *can* stream,
  just not on POST.
- `mcp_sse` (line 199) **drops the request-side session_id** in a
  drive-by, with a comment that says "for parity with today's behavior".
  No one knows why.
- `passthrough` (line 286) re-uses `UpstreamUrlMapMiddleware` — a
  ResponseMiddleware type — but calls it directly (not through a chain).
  This is the only place that middleware runs. So the "middleware trait"
  has one consumer outside the main chain.
- Error handling: all three handlers have near-identical
  "return 502 + emit event + tag" blocks that could be one function.
  Instead they're copy-pasted.

### 1.4 `ResponseContext` conflates two things

`pipeline/context.rs:62-91` holds `body: Vec<u8>` **and** `json: Option<Value>`
as parallel, potentially-divergent state. The invariant "they represent
the same bytes" is maintained by convention (middleware order + `json_mutated`
flag). Breaking that invariant is easy and fails silently.

Three ways the invariant breaks:

1. A middleware calls `resp.json.as_mut()` (not `json_mut()`) and
   mutates without setting the flag → `EncodeResponseJson` skips
   reserialization → body goes out stale.
2. A middleware calls `resp.body = something` directly → `resp.json`
   still holds the old parse, but nothing notices.
3. A middleware short-circuits by returning early without updating
   state → later middlewares see partial data.

Enforceable only by discipline, not by types. Every new middleware is a
potential bug site.

### 1.5 `build_response` silently drops headers

`forwarding.rs:90-114` copies **four** headers from upstream:
`content-type`, `cache-control`, `mcp-session-id`, `www-authenticate`.
Everything else is discarded. This is probably deliberate (strips
hop-by-hop, `set-cookie`, etc.) but:

- **No tests assert which headers are stripped**. A future contributor
  copy-pastes this loop and adds `content-length` — now we're lying
  about buffered vs chunked in a different way.
- **CORS headers** are not copied from upstream; they're re-added by
  `tower-http::CorsLayer` at the axum layer.
- **No way to add a new passthrough header** without editing this
  function directly.
- **`content-length` is computed by axum from the body bytes**, which
  is why SSE becomes non-chunked — the buffered body tells axum to
  announce its length.

### 1.6 Error paths are inconsistent

In `mcp_post` (run.rs:92-108), an upstream error returns a 502 with a
plain-text body `"Upstream error: {e}"`. In `mcp_sse` (line 249),
likewise. In `passthrough` (line 333), likewise. In any of these, if
the request has a session id, **the session touch in the request
middleware already happened** — we've registered activity on a session
whose request never actually reached upstream. Subtle for observability.

### 1.7 Event emission timing

`emit_request_event` is called from 7+ places in `run.rs` alone
(success, error, each handler). Every caller builds a `ResponseSummary`
with slightly different fields set. Easy to forget `rpc_error`,
easy to include it twice.

Consolidating would help but the real issue is that **event fields are
eagerly populated even when no sink consumes them** (audit issue #5).
Fixing event emission while the handlers remain scattered is playing
whack-a-mole.

---

## Part 2 — What makes the code hard to maintain

### 2.1 Middleware trait does too many different things

Under `ResponseMiddleware` we have:

- Data transforms: `UrlRewriteMiddleware`, `WidgetOverlayMiddleware`,
  `UpstreamUrlMapMiddleware`, `DecodeResponseJson`, `EncodeResponseJson`,
  `SseUnwrap`/`SseWrap` (pre-rename).
- Observability: `SchemaIngestMiddleware`, `StaleMarkMiddleware`,
  `SessionStartMiddleware`.
- Health tracking: `McpHealthMiddleware`.

Three very different concerns, one trait. Reading `run.rs:137-160`
doesn't tell you which are transforms, which are side-effects, which
can no-op safely. To know what happens to a response, you have to open
eight files.

### 2.2 Test boilerplate is duplicated per file

Each middleware test file reconstructs a full `ProxyState` with 14
fields (see `middleware/sse.rs:65-95` or any other test module). Adding
a field to `ProxyState` breaks every test module. Refactoring a
middleware requires re-editing its 50-line test setup.

Now that I've duplicated the `test_state` helper into `encode_tests`
specifically to dodge a Rust visibility quirk, there are two copies of
the same ~25-line function in the same file. That's a smell.

### 2.3 Ordering dependencies are tested by a single canary, not by types

Middleware order matters: `DecodeResponseJson` must run before anything
reads `resp.json`; `EncodeResponseJson` must run last; `SessionStart`
reads JSON so it must come after `DecodeResponseJson`; etc. This is
enforced by one test at `pipeline/run.rs:465` that asserts the chain
runs in a specific order.

The compiler doesn't help here. Swap two middlewares, the test might
miss the regression (because the test checks observable output, not
intermediate state).

### 2.4 Adding a new behavior requires touching ~6 files

To add, say, a request-rate-limiting header transform, you need to:

1. Create a new middleware type + file
2. Implement `ResponseMiddleware` for it
3. Register it in `middleware/mod.rs`
4. Insert it at the correct position in `run.rs:137-160`
5. Update `run.rs:465` ordering canary test
6. Write test harness with full `ProxyState` scaffolding

Five of those six aren't about the new behavior — they're about the
framework. Ceremony per feature is too high.

### 2.5 Passthrough "rewrite" is hidden

`passthrough` in run.rs:288-290 runs `UpstreamUrlMapMiddleware`, which
UTF-8-decodes the body and does string replacement. It's labeled
`"rewritten"` or `"passthrough"` in the event tag based on whether the
body is JSON. Operators reading the event log think "passthrough" means
"bytes unchanged" — but it doesn't, always.

### 2.6 No place to put cross-cutting concerns cleanly

Request-level metrics, structured tracing spans, per-request deadlines,
auth checks — all would need to hook into the pipeline somehow. The
current trait signatures don't give them a natural home. A middleware
is either synchronous data transform OR async side-effect, and the
pipeline doesn't know which.

---

## Part 3 — Target architecture

Branch-by-shape, inspired by Pingora. Two primitives:

- **Header phase** (cheap, synchronous, always runs): session tracking,
  capture `mcp-session-id` from upstream response, decide on a body
  strategy.
- **Body strategy** (one of three): `StreamThrough`, `BufferAndMaybeRewrite`,
  `BufferAndSubstitute`. Each strategy is a concrete function, not a
  composition.

```
┌──────────────────────────────────────────────────────────┐
│                     axum fallback handler                │
└─────────────────────────┬────────────────────────────────┘
                          ▼
                  parse_request(req)
                          │
                  header-phase steps
                  (session touch, delete, etc.)
                          │
                  classify(request)
                          ▼
        ┌─────────────────┼─────────────────┐
        ▼                 ▼                 ▼
 LocalWidget         ForwardAndBuffer   ForwardAndStream
 (serve HTML)        (rewrite-capable)  (everything else)
        │                 │                 │
        │                 ▼                 │
        │         classify_response         │
        │        (content-type, markers)    │
        │                 │                 │
        │         ┌───────┴───────┐         │
        │         ▼               ▼         │
        │   rewrite_json()   pass_through() │
        │         │               │         │
        └─────────┴────────┬──────┴─────────┘
                           ▼
                  emit_request_event()
                           ▼
                  return Response
```

### 3.1 Core types

```rust
/// Classification of a request determines which body strategy applies.
/// Decided pre-forward based on method and path.
enum RequestKind {
    /// POST /mcp with a method that could mutate response bodies
    /// (tools/*, resources/*). Forward, buffer response, check if
    /// actual rewrite is needed.
    McpPostRewriteCapable(McpMethod),

    /// POST /mcp with a method that never needs mutation
    /// (initialize, ping, notifications/*, prompts/*, etc). Stream.
    McpPostPassthrough(McpMethod),

    /// GET /mcp. Streaming SSE channel.
    McpSseStream,

    /// DELETE /mcp. Session end — no body to process.
    McpSessionEnd,

    /// ui://widget/... or other local asset paths.
    LocalWidget { name: String },

    /// Anything else. Stream.
    Passthrough,
}

/// Body strategy decided after looking at the upstream response headers.
enum BodyStrategy {
    /// Stream upstream body bytes to client unchanged. No buffering.
    StreamThrough,

    /// Buffer the body, scan for rewrite markers, either rewrite or
    /// pass through. Only used for McpPostRewriteCapable.
    BufferAndMaybeRewrite { method: McpMethod },

    /// Buffer, locate widget content, substitute local HTML.
    BufferAndSubstituteWidget,
}
```

### 3.2 Entry point — one flat function replacing `run()` + three handlers

```rust
pub async fn run(state: Arc<ProxyState>, req: Request) -> Response {
    let ctx = parse_request(&req);

    // Header phase — cheap, no body access.
    if let Some(short_circuit) = header_phase(&state, &ctx) {
        return short_circuit;
    }

    let kind = classify_request(&ctx);
    match kind {
        RequestKind::LocalWidget { name } => serve_widget_html(&state, &name).await,
        RequestKind::McpSessionEnd => handle_session_end(&state, &ctx).await,
        RequestKind::McpSseStream => stream_sse(&state, &ctx, req).await,
        RequestKind::McpPostPassthrough(method) => forward_and_stream(&state, &ctx, method, req).await,
        RequestKind::McpPostRewriteCapable(method) => forward_and_buffer(&state, &ctx, method, req).await,
        RequestKind::Passthrough => forward_and_stream(&state, &ctx, McpMethod::Unknown, req).await,
    }
}
```

### 3.3 Three body strategies — explicit, concrete, non-composable

```rust
async fn forward_and_stream(state, ctx, method, req) -> Response {
    let upstream = forward(state, ctx, req).await?;
    let (parts, body) = upstream.into_parts();
    update_session_from_headers(ctx, &parts.headers);
    emit_request_event(state, ctx, /* no body info */);
    build_streaming_response(parts, body)
}

async fn forward_and_buffer(state, ctx, method, req) -> Response {
    let upstream = forward(state, ctx, req).await?;
    let (parts, body) = upstream.into_parts();
    update_session_from_headers(ctx, &parts.headers);

    // Pre-scan: does this body even contain rewrite markers?
    let bytes = aggregate_body(body, state.max_response_body).await?;
    let strategy = classify_response(&parts.headers, &bytes, method);

    let final_bytes = match strategy {
        BodyStrategy::StreamThrough => bytes,
        BodyStrategy::BufferAndMaybeRewrite { method } => {
            maybe_rewrite(&state.rewrite_config, method, bytes)
        }
        BodyStrategy::BufferAndSubstituteWidget => {
            substitute_widget(&state, ctx, bytes).await
        }
    };

    emit_request_event(state, ctx, Some(final_bytes.len()));
    build_buffered_response(parts, final_bytes)
}

async fn stream_sse(state, ctx, req) -> Response {
    // GET /mcp — always streaming.
    let upstream = forward_get(state, ctx, req).await?;
    let (parts, body) = upstream.into_parts();
    emit_request_event(state, ctx, None);
    build_streaming_response(parts, body)
}
```

### 3.4 Rewrite marker pre-scan

```rust
const REWRITE_MARKERS: &[&[u8]] = &[
    b"connect_domains",  b"resource_domains",  b"frame_domains",
    b"connectDomains",   b"resourceDomains",   b"frameDomains",
    b"openai/widgetCSP", b"ui.csp",            b"openai/widgetDomain",
];

fn has_rewrite_marker(body: &[u8]) -> bool {
    REWRITE_MARKERS.iter().any(|m| memchr::memmem::find(body, m).is_some())
}

fn maybe_rewrite(config: &ArcSwap<RewriteConfig>, method: McpMethod, bytes: Bytes) -> Bytes {
    if !has_rewrite_marker(&bytes) {
        return bytes;  // fast path — most tool-call responses
    }
    let Ok(mut value) = serde_json::from_slice(&bytes) else {
        return bytes;  // not valid JSON — pass through
    };
    if rewrite_response(method, &mut value, &config.load()) {
        Bytes::from(serde_json::to_vec(&value).unwrap_or(bytes.to_vec()))
    } else {
        bytes
    }
}
```

### 3.5 What this kills from the current design

| Current thing                                    | Fate in refactor                  |
|--------------------------------------------------|-----------------------------------|
| `ResponseMiddleware` trait                       | Gone. Replaced by concrete functions. |
| `RequestMiddleware` trait                        | Kept but slimmed — only for header-phase work. |
| `DecodeResponseJson` / `EncodeResponseJson`      | Gone. Inlined into `maybe_rewrite`. |
| `SseUnwrapMiddleware` / `SseWrapMiddleware`      | Already renamed → gone. |
| `UpstreamUrlMapMiddleware`                       | Gone — merges into passthrough conditional logic. |
| `McpHealthMiddleware`                            | Becomes a function called from `forward_and_*`. |
| `SessionStartMiddleware` (response-side)         | Moves to `update_session_from_headers`. |
| `SchemaIngestMiddleware` / `StaleMarkMiddleware` | Kept as functions, called only in buffer-path. |
| `WidgetOverlayMiddleware`                        | Stays as a function called in the widget-substitution strategy. |
| Middleware ordering canary test                  | Gone — no more ordering to canary. |
| `ResponseContext.body` + `.json` dual state      | Gone — `Bytes` goes through `maybe_rewrite` as a single value. |
| `ResponseContext.json_mutated` band-aid          | Gone — `rewrite_response` returning `bool` does the same thing but scoped to one function. |
| `read_body_capped` unconditional                 | Called only by `forward_and_buffer`. Streaming path doesn't buffer. |
| 3× copy-pasted error handling                    | One `forward_or_502` helper. |

### 3.6 What this improves in the hot-path issues doc

- **Issue #1 (SseUnwrap body clone)** — gone. No universal decode bracket.
- **Issue #2 (SseWrap reserialize)** — gone. Reserialize only when
  `rewrite_response` returns true.
- **Issue #3 (RwLock per request)** — much smaller. Only hit on the
  rewrite path, which is the minority. Combine with `arc_swap` and
  it's effectively free.
- **Issue #4 (UpstreamUrlMap full scan)** — merges into the
  classification step. Only runs when there's an actual JSON body
  that could contain the upstream URL.
- **Issue #5 (eager event alloc)** — orthogonal, but now easier to
  fix because emission is one place.
- **Issue #7 (response body Vec copy)** — gone on the streaming path;
  on the buffer path, the `Bytes` is passed through without the
  `.to_vec()` copy.

Five audit items collapse into one architectural shift.

---

## Part 4 — Implementation plan

Six phases. Each phase is a landing PR. The pipeline stays **buildable
and correct** at every phase — no big-bang cutover. Bench after each
phase to catch regressions.

### Phase 0 — set up guardrails (0.5 day)

Before touching anything:

1. **Add `scripts/scenarios/sse-compat.sh` to CI**. It already detects
   SSE breakage; make it a hard gate. Commit the expected output for
   PASS so regressions fail loudly.
2. **Add `scripts/scenarios/multi-event-sse.sh`** — new scenario that
   drives mcpr with a mock emitting 3 SSE events (progress + result).
   Today this silently breaks. The scenario should fail before the
   refactor and pass after.
3. **Add `scripts/scenarios/passthrough-binary.sh`** — scenario that
   forwards a binary response (e.g. PNG bytes) through mcpr. Asserts
   the client receives byte-identical output. Today this may corrupt
   via `from_utf8_lossy`.
4. **Seed a multi-run harness** — `--runs N` flag on bench scripts
   that reports median + MAD. We need it before we can trust any
   perf comparisons.

### Phase 1 — add `classify_request` + streaming GET SSE (already works, just lift) (1 day)

1. Extract `classify_request` as a pure function. No behavior change.
2. Rename `mcp_sse` → `stream_sse`, move to its own module, no logic
   change. Reference test: streaming path works end-to-end.
3. Rename `passthrough` → `stream_passthrough_if_possible`, mark the
   `UpstreamUrlMapMiddleware` call for removal in phase 4.

### Phase 2 — introduce `ArcSwap<RewriteConfig>` (0.5 day)

1. Add `arc-swap = "1"` to mcpr-core.
2. Change `ProxyState.rewrite_config` from `Arc<RwLock<RewriteConfig>>`
   to `Arc<ArcSwap<RewriteConfig>>`.
3. All call sites change from `.read().await` to `.load()` (sync).
4. This unblocks the refactor — no more async lock on the hot path.
5. Bench — expect p95 to tighten modestly.

Audit issue #3 dies here.

### Phase 3 — implement `forward_and_stream` path for rewrite-incapable POST methods (1 day)

1. Add `McpMethod::is_rewrite_capable()` → returns true only for
   `tools/*`, `resources/*`. Everything else is
   `McpPostPassthrough`.
2. Route those requests through the existing streaming codepath
   (copied from `stream_sse`, body kept as `Body::from_stream`).
3. Do NOT change the POST rewrite-capable path yet — it still uses
   the old middleware chain.
4. Bench — `initialize`, `ping`, `notifications/*` should show
   material improvement (no more buffering).
5. Re-run `sse-compat.sh` with an `initialize` request — confirm
   byte-for-byte match, chunked preservation.

Audit issues #1, #2 start dying here (for the non-rewrite methods).

### Phase 4 — implement `forward_and_buffer` with marker-scan + `rewrite_response` bool return (1.5 days)

This is the hairy one.

1. Introduce `classify_response(headers, bytes, method) -> BodyStrategy`.
   Pure function. Unit-testable without axum.
2. Implement `maybe_rewrite` using the marker scan.
3. Wire the rewrite-capable methods through `forward_and_buffer`,
   bypassing the old middleware chain. Keep the old chain working
   for now (behind a feature flag or a runtime switch).
4. Integration-test both paths against the same fixtures. Deltas should
   be byte-for-byte identical for responses with no rewrite markers.
5. Switch default to new path. Delete old chain code.

Audit issues #4, #7 die here. #1 and #2 are fully gone at this point.

### Phase 5 — delete dead code + tidy (1 day)

1. Delete `DecodeResponseJson`, `EncodeResponseJson`,
   `UpstreamUrlMapMiddleware`, and `ResponseMiddleware` trait.
2. Convert remaining response-side things (`SessionStartMiddleware`,
   `McpHealthMiddleware`, `SchemaIngestMiddleware`,
   `StaleMarkMiddleware`) from traits to functions called explicitly
   by the buffer path.
3. Consolidate the 3× error handling into one `forward_or_502` helper.
4. Delete `ResponseContext.json`, `json_mutated`, `was_sse`, `rpc_error`.
   The buffer path carries `Bytes` + an optional `Value` as locals.
5. Reduce `build_response` to a thin wrapper over
   `Response::from_parts`, since header filtering moves into two
   small helpers (`filter_upstream_headers_for_stream`,
   `filter_upstream_headers_for_buffer`).

### Phase 6 — event emission consolidation + lazy fields (0.5 day)

Now that emit is called from 3 handlers (not 7), it's cheap to fix
issue #5: gate `build_request_event` on "does the bus have any active
sinks?" If not, emit a minimal event. If yes, build the full one.

---

## Part 5 — Risks and how we catch them

| Risk                                            | Mitigation                                              |
|-------------------------------------------------|----------------------------------------------------------|
| Rewrite subtle behavior change (key ordering)   | Phase 0.1 — `sse-compat.sh` in CI; phase 4 — side-by-side fixture diff |
| Binary passthrough corruption                   | Phase 0.3 — new scenario                                 |
| Multi-event SSE silent drop                     | Phase 0.2 — new scenario                                 |
| Session tracking regression                     | Existing tests in `session.rs`; add integration test for POST with upstream-assigned session id |
| Perf regression on a specific method            | Phase 0.4 — multi-run harness; bench after each phase    |
| Unused markers falsely triggering buffer path   | Test: a response with `{"connect_domains": []}` as a literal string in a `text` field — should NOT re-parse |
| Operator observability changes                  | Keep event field set identical; only change the work done |

## Part 6 — What this does NOT fix

Be honest about scope. After this refactor:

- **Event bus backpressure / sqlite sink** — still a potential tail
  contributor (audit issue #5). Refactor makes it easier to isolate
  but doesn't fix it.
- **CORS headers added unconditionally** — still at the axum layer.
  Not scope.
- **Request body buffered for forwarding** — POST bodies are still
  read into `Bytes` before forwarding. Streaming request bodies
  is a separate refactor.
- **No per-event SSE transforms** — if we ever need to mutate SSE
  events in-flight (say, URL-rewrite inside streamed events), we
  need a per-chunk filter API. Not needed for current features.
- **Multi-run bench statistics** — Phase 0 adds `--runs N`, but the
  benches themselves still run on a noisy dev laptop. Credible public
  numbers need a pinned CI runner eventually.

## Part 7 — Total cost estimate

Roughly 5 working days end-to-end, landable in 6 PRs. Each PR is
small enough to review in < 30 minutes. Each PR leaves the system
working. No branch-long feature freeze required.

Audit issues closed: **#1, #2, #3, #4, #7 (five critical/high)**.
Audit issues that become easier to finish: **#5, #6** (they become
local changes instead of whole-pipeline changes).
