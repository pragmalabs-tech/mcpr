# Pipeline Refactor Plan

Migration from the current proxy pipeline to the architecture in [`PIPELINE_ARCHITECTURE.md`](PIPELINE_ARCHITECTURE.md). No backward compatibility — aggressive deletion of anything that doesn't fit the target design.

## Executive summary

Six phases, each ending on a green build. Phases 1 and 2 are independent and can land in either order. Phase 3 is the widest (middleware implementations). Phases 4–5 swap the pipeline engine. Phase 6 is post-cutover cleanup.

Session subsystem refactoring (rename, state collapse, reaper) and server-push plumbing (`ServerPushBroker`) are deliberately **out of scope**. Middlewares call existing subsystems (`MemorySessionStore`, `SchemaManager`, `ProxyHealth`, `rewrite::has_markers/rewrite_in_place`) through their current APIs. Those subsystems can be refactored in separate tracks later — they do not block the pipeline refactor.

Target diff: net **reduction** in LOC. The current `pipeline/` + `handlers/` + `steps/` + widgets amount to ~2500 LOC; the target shape is closer to ~1500 LOC with richer type invariants.

## Guiding principles

- **Green build at every phase boundary.** Within a phase, feel free to break things; across phases, `cargo build` and `cargo test` pass.
- **Delete as we go.** Each phase has an explicit delete list. Do not carry dead code "just in case."
- **No feature flags, no compatibility shims.** This is a clean break; the user has confirmed no back-compat is needed.
- **Atomic cutover for the pipeline engine.** Old and new engines are *not* coexisting in production — there is one moment where we swap `handle_request` to the new pipeline and delete the old.
- **Update docs with code.** `CSP.md`, `PROXY_CONFIGURATION.md`, and any README references come out in the same commit as the code they document.

---

## Phase 1 — Widget static-serving purge (keep CSP)

**Goal:** Delete the static-asset serving path and everything that exists only to support it. **CSP rewriting stays** — it is first-class configuration that rewrites upstream MCP responses carrying widget CSP directives, and it must survive this refactor with hot-reload intact. Self-contained; no dependency on the rest of the refactor.

### Delete

Code (widget static serving only):
- `mcpr/crates/mcpr-core/src/proxy/widgets.rs` (entire module — `serve_widget_html`, `list_widgets`, `serve_widget_asset`, `WidgetSource`, MIME detection).
- `mcpr/crates/mcpr-core/src/proxy/pipeline/steps/widget.rs` — `maybe_overlay`.
- `RequestKind::WidgetHtml | WidgetList | WidgetAsset` variants in `pipeline/route.rs`.
- Widget branches in `pipeline/run.rs` (the three `RequestKind::Widget*` match arms).
- The `widget::maybe_overlay()` call in `handlers/buffered.rs`.
- `has_widgets` parameter threaded through `classify_request`.
- `pub mod widgets` + `pub mod widget` declarations in their parent `mod.rs` files.

Config:
- `widgets: Option<String>` field in `mcpr-cli/src/config.rs` (pointed at a local widget source — no longer served).
- `publicWidgetDomain`.

**Keep (explicitly):**
- `proxy/csp.rs` (`CspConfig`, `DirectivePolicy`, `Mode`, `WidgetScoped`, `effective_domains`).
- `proxy/rewrite.rs` (`RewriteConfig`, `rewrite_response`).
- `proxy/pipeline/steps/rewrite.rs` (`has_markers`, `rewrite_in_place`) — stays in place this phase; moves into `CspRewriteMiddleware` in Phase 3.
- `[csp]` and `[[csp.widget]]` config sections (widget-scoped entries match resource URIs like `ui://widget/payment*`, not local widget endpoints).
- `ArcSwap<RewriteConfig>` hot-reload plumbing.

Tests (remove outright):
- `pipeline/run.rs` widget integration test (`resources_read_overlays_widget_html_from_static_source`).
- `widgets.rs` — MIME / asset rewrite tests (delete with the file).
- `route.rs` — 6 classification tests covering `WidgetHtml/List/Asset`.
- Any integration tests under `mcpr/tests/` that exercise widget *static serving* paths. CSP-rewrite tests stay.

Docs:
- Widget-serving sections in `mcpr/docs/proxy/PROXY_CONFIGURATION.md`.
- Widget-serving references in `mcpr/README.md` if any.
- `mcpr/docs/proxy/CSP.md` stays; update it to describe CSP rewriting as the response-side middleware it becomes in Phase 3, and to drop any mention of local widget serving.

### Add

- A short note in `PROXY_CONFIGURATION.md` stating that widget *serving* was removed (link to spec decision) and that CSP rewriting of upstream responses continues to apply.

### Acceptance

- `cargo build` + `cargo test` green across all crates.
- No mention of `WidgetHtml`, `WidgetList`, `WidgetAsset`, `serve_widget_html`, `publicWidgetDomain`, or the `widgets =` config key anywhere in code.
- `CSP.md`, `CspConfig`, `rewrite_response`, `has_markers`, and the `[csp]` config section are still present and exercised by tests.
- `mcpr proxy run` against any `mcpr.toml` that previously used widget-serving keys (`widgets`, `publicWidgetDomain`) now fails with a clear error (unknown field). A `mcpr.toml` with a `[csp]` block still loads and applies.

### Estimated size

One PR, ~-600 LOC net.

---

## Phase 2 — Type foundation

**Goal:** Introduce the new type system (`Request`, `Response`, `Route`, `Context`, `JsonRpcEnvelope`, `McpMessage`, `ClientKind`, `ServerKind`, etc.) directly in the existing `pipeline/` module under non-colliding filenames. Nothing uses them yet. See [`PHASE_2_PLAN.md`](PHASE_2_PLAN.md) for the file-by-file design.

### Add

Six new files under `mcpr/crates/mcpr-core/src/proxy/pipeline/`:

- `envelope.rs` — `JsonRpcEnvelope`, `JsonRpcId`, `JsonRpcError`, `ParseError`, `JsonRpcEnvelope::parse`, `params_as::<T>()`, `result_as::<T>()`.
- `message.rs` — `McpMessage`, `ClientKind`, `ClientMethod`, `ClientNotifMethod`, `ServerKind`, `ServerMethod`, `ServerNotifMethod`, plus `classify_client` / `classify_server`. Every method enum has a trailing `Unknown(String)` so non-spec methods forward unchanged.
- `values.rs` — `Request`, `Response`, `Route`, `BufferPolicy`, `Envelope`, `Context`, `Intake`, `Working`, `McpRequest`, `McpTransport`, `OAuthRequest`, `RawRequest`.
- `middleware.rs` — `RequestMiddleware`, `ResponseMiddleware` traits, `Flow` enum (annotated with `#[async_trait]`).
- `driver.rs` — `Pipeline` driver + `Router` and `Transport` traits. Unit tests with fake middlewares exercising `Flow::Continue` / `Flow::ShortCircuit`, and one smoke test through a full stub chain.
- `stubs.rs` — placeholders (`SessionId`, `SessionRecord`, `UrlMap`, `OAuthKind`, `TagSet`) for types later phases fill in or replace with references to existing subsystems. Each item carries a `// phase-N` marker so later phases can remove by grep.

Registration: six new `pub mod …;` lines in `pipeline/mod.rs`. No other file is edited.

### Delete

Nothing. Old `pipeline/run.rs`, `context.rs`, `route.rs`, `parser.rs`, `emit.rs`, `handlers/`, `steps/` stay byte-identical. Phase 6 deletes them.

### Acceptance

- `cargo build` + `cargo test` green. All new test modules (`envelope::tests`, `message::tests`, `driver::tests`) pass.
- No production code outside the six new files references the new symbols. `rg 'pipeline::(envelope|message|middleware|driver|values|stubs)::' mcpr/crates` returns matches only from inside those files.
- Reviewers can read `values.rs` + `message.rs` and map each type back to a section in `PIPELINE_ARCHITECTURE.md`.

### Estimated size

One PR, ~+1000 LOC added, 0 deleted.

### Why no `pipeline2/`

Earlier drafts of this plan used a parallel `pipeline2/` module that renamed to `pipeline/` in Phase 6. That approach was dropped: none of the new type names collide with existing symbols, flat filenames (`envelope.rs`, `message.rs`, `values.rs`, …) are distinct from the old file names (`run.rs`, `context.rs`, `route.rs`, `parser.rs`, `emit.rs`), and Phase 6's rename step disappears.

---

## Phase 3 — Middleware implementations

**Goal:** Port every current `steps/*.rs` free function to a middleware struct implementing `RequestMiddleware` or `ResponseMiddleware` from Phase 2. Each middleware calls existing subsystems directly — no new `SessionManager`, `ServerPushBroker`, or `subsystems/` module in this refactor.

### Existing subsystems used (no changes)

- `MemorySessionStore` (`protocol/session.rs`) — called via its async API: `state.sessions.touch(id).await`, `.create(id).await`, `.set_client_info(id, info).await`, `.update_state(id, s).await`, `.remove(id).await`, `.get(id).await`.
- `SchemaManager` (`protocol/schema_manager/`) — existing `ingest` method.
- `ProxyHealth` / `SharedProxyHealth` (`proxy/health.rs`) — existing `record` / `record_upstream_error` methods.
- `rewrite.rs` (`has_markers`, `rewrite_in_place`) — moved *from* `pipeline/steps/rewrite.rs` into `CspRewriteMiddleware`.
- `ArcSwap<RewriteConfig>` — hot-reload plumbing unchanged.

### Add

`mcpr/crates/mcpr-core/src/proxy/middlewares/` — one file per middleware:

**Request side:**
- `session_delete.rs` — `SessionDeleteMiddleware`. Matches `Request::Mcp(_)` + `DELETE`. Forwards DELETE upstream (temporarily via existing `forward_request`), calls `state.sessions.remove(id).await`, emits `ProxyEvent::SessionEnd`, short-circuits 204.
- `session_touch.rs` — `SessionTouchMiddleware`. Matches `Request::Mcp(_)` with session id, calls `state.sessions.touch(id).await`, stashes looked-up `SessionInfo` on `cx.working.session`.
- `client_info_inject.rs` — `ClientInfoInjectMiddleware`. Matches `ClientKind::Request(ClientMethod::Lifecycle(_))`, deserializes `InitializeParams` via `params_as::<T>()`, stores in `cx.working.client`.

**Response side:**
- `schema_ingest.rs` — `SchemaIngestMiddleware`. Matches `McpBuffered` where originating `ClientKind` was a list method; calls `SchemaManager::ingest`.
- `session_record.rs` — `SessionRecordMiddleware`. Matches `McpBuffered` for Initialize + status < 400. Captures `mcp-session-id` header, calls `state.sessions.create(id).await` + `set_client_info`, emits `ProxyEvent::SessionStart`.
- `health_track.rs` — `HealthTrackMiddleware`. Matches `McpBuffered | McpStreamed | Upstream502` → calls the existing `ProxyHealth::record`.
- `csp_rewrite.rs` — `CspRewriteMiddleware`. Matches `McpBuffered` where originating `ClientKind` was `Tools::List | Tools::Call | Resources::Read`. Wraps the existing `has_markers` byte pre-scan and `rewrite_in_place` logic; holds an `Arc<ArcSwap<RewriteConfig>>` so `mcpr.toml` reloads swap the inner `Arc` without restarting the middleware. Mutates the `McpMessage`'s result in place; seal reserializes.
- `url_map.rs` — `UrlMapMiddleware`. Matches `OauthJson`.
- `envelope_seal.rs` — `EnvelopeSealMiddleware`. Matches `McpBuffered`, serializes `McpMessage` once, re-wraps as SSE if `Envelope::Sse`.

Not included — deferred to separate work:
- `ServerPushSubscribeMiddleware` / `ServerPushPublishMiddleware` — require a `ServerPushBroker` that does not exist. Legacy SSE path stays a byte passthrough for now.

Each file: the struct, `impl RequestMiddleware` / `impl ResponseMiddleware`, unit tests exercising the variant matches it cares about + the happy path against a mock / fake.

### Delete

Nothing yet — old `steps/*.rs` still live, not called by the new middlewares.

### Acceptance

- Each middleware has unit tests with >80% branch coverage.
- `cargo clippy` clean.
- Old `steps/*.rs` is still present and still called by the old pipeline.

### Estimated size

One large PR or ~2–3 smaller ones, ~+700 LOC additions.

---

## Phase 4 — Router + Transport extraction

**Goal:** Split the fused `classify_request` + handler dispatch into (a) a pure `Router` that returns a `Route`, and (b) a `Transport` that is the only layer touching the network.

### Add

- `mcpr/crates/mcpr-core/src/proxy/router.rs` — `Router` struct + `fn route(&Request, &Config) -> Route`. Pure. `BufferPolicy` table lives here (not on `McpMethod`). Unit tests cover every `ClientMethod` variant → expected `Route`.
- `mcpr/crates/mcpr-core/src/proxy/transport.rs` — `Transport` struct wrapping `UpstreamClient`. One async `dispatch(Request, Route, &Context) -> Response`. Maps reqwest errors to `Response::Upstream502`. Streamed vs buffered decision comes from `Route`, not from content-type sniffing.
- Intake: `mcpr/crates/mcpr-core/src/proxy/intake.rs` — `FromRequest` impl (or free function) that turns axum parts into a `Request` via content-based classification. Replaces `build_request_context` + `classify_request`.

### Delete

Nothing yet — old `classify_request`, `forwarding.rs`, handler dispatch still in place.

### Acceptance

- `cargo test` green.
- New router / transport / intake are unit-tested in isolation (no live network for router; mocked reqwest client for transport).

### Estimated size

One PR, ~+400 LOC.

---

## Phase 5 — Atomic cutover

**Goal:** Flip the entry point from the old pipeline to the new one. Delete everything the new pipeline replaces. This is the destructive phase.

### Change

- `mcpr/crates/mcpr-cli/src/proxy.rs::handle_request` now constructs a `Context`, calls `intake::from_axum_parts`, and runs `Pipeline::run`. Uses `IntoResponse` on `Response` to return the axum response.
- Pipeline construction (in `ProxyState::new` or equivalent) builds the ordered middleware chain with registration logging.

### Delete

Old pipeline engine:
- `mcpr/crates/mcpr-core/src/proxy/pipeline/run.rs` (the `pub async fn run`).
- `mcpr/crates/mcpr-core/src/proxy/pipeline/context.rs` (`RequestContext`).
- `mcpr/crates/mcpr-core/src/proxy/pipeline/parser.rs` (`build_request_context`).
- `mcpr/crates/mcpr-core/src/proxy/pipeline/route.rs` (`RequestKind`, `classify_request`).
- `mcpr/crates/mcpr-core/src/proxy/pipeline/emit.rs` (logic moves to the new `Emit` stage, which is a final response middleware or a dedicated post-chain call).
- Entire `mcpr/crates/mcpr-core/src/proxy/pipeline/handlers/` directory (`mod.rs`, `header_phase.rs`, `buffered.rs`, `streamed.rs`, `sse.rs`, `passthrough.rs`, handler helpers).
- Entire `mcpr/crates/mcpr-core/src/proxy/pipeline/steps/` directory (all `.rs` files).

Protocol layer:
- `McpMethod::needs_response_buffering` method (policy moved to router table).
- `parse_message` (was never called externally; only inside `parse_body`).
- `parse_body` + `ParsedBody` struct (replaced by `JsonRpcEnvelope`).
- `McpMethod` enum — replaced by `ClientMethod` / `ServerMethod`.

Forwarding:
- `mcpr/crates/mcpr-core/src/proxy/forwarding.rs` — replaced by `transport.rs`. (`UpstreamClient` may live on if still shared; otherwise it folds into `Transport`.)

### Acceptance

- `cargo build` + `cargo test` green.
- Integration tests (if any remain for session lifecycle, schema ingest, upstream error handling) pass.
- Manual smoke: `mcpr proxy run` against a live upstream, verify `tools/list` / `tools/call` / initialize flow + session touch + dashboard event emission.
- `rg "RequestContext|RequestKind|ParsedBody|McpMethod" mcpr/crates` returns zero matches.

### Estimated size

One large PR, ~-1500 LOC net (deletes dominate).

---

## Phase 6 — Post-cutover cleanup

**Goal:** Polish. Lean on tower where we still have hand-rolled primitives. Tighten what Phase 5 left behind.

### Changes

- Replace hand-rolled concurrency semaphore (`UpstreamClient::semaphore`) with `tower::limit::ConcurrencyLimitLayer` wrapping the axum service.
- Replace ad-hoc upstream timeout logic with `tower_http::timeout::TimeoutLayer` where the call path allows; keep `reqwest`'s per-call timeout only for streaming responses where the tower layer can't reach.
- Add a `tower_http::trace::TraceLayer` at the axum boundary (we already log registered middlewares; spans add request-scoped context for logs).
- Replace any remaining `emit_upstream_error` style callsites with `Response::Upstream502` returns so they flow through the response middleware chain.
- Update `mcpr/docs/proxy/PRODUCTION_GUIDE.md` to reference the new pipeline structure at a high level (avoid re-documenting internal types; link to `PIPELINE_ARCHITECTURE.md`).
- Move `PIPELINE_ARCHITECTURE.md` + `PIPELINE_REFACTOR_PLAN.md` from repo root into `mcpr/docs/proxy/` — that's their permanent home.

### Delete

- Any `emit_upstream_error`-shaped helpers in `handlers/mod.rs` that survived Phase 5 by accident.
- Hand-rolled semaphore / timeout code superseded by tower layers.

### Acceptance

- `cargo build --all-features` + `cargo test` green.
- Runtime behavior unchanged from end-of-Phase-5 (smoke-tested).
- `cargo tree` shows `tower-http` features only for what we actually use (timeout, trace).

### Estimated size

One PR, ~-200 LOC.

---

## Consolidated delete list

By end of Phase 6, these disappear entirely:

### Files

- `proxy/widgets.rs`
- `proxy/pipeline/run.rs`
- `proxy/pipeline/context.rs`
- `proxy/pipeline/parser.rs`
- `proxy/pipeline/route.rs`
- `proxy/pipeline/emit.rs`
- `proxy/pipeline/handlers/mod.rs`
- `proxy/pipeline/handlers/header_phase.rs`
- `proxy/pipeline/handlers/buffered.rs`
- `proxy/pipeline/handlers/streamed.rs`
- `proxy/pipeline/handlers/sse.rs`
- `proxy/pipeline/handlers/passthrough.rs`
- `proxy/pipeline/steps/mod.rs`
- `proxy/pipeline/steps/health.rs`
- `proxy/pipeline/steps/rewrite.rs` (logic migrated into `middlewares/csp_rewrite.rs` in Phase 3; the steps file is deleted in Phase 5 along with the rest of `steps/`)
- `proxy/pipeline/steps/schema.rs`
- `proxy/pipeline/steps/session.rs`
- `proxy/pipeline/steps/url_map.rs`
- `proxy/pipeline/steps/widget.rs`
- `proxy/forwarding.rs`

Kept (explicit anti-delete): `proxy/csp.rs`, `proxy/rewrite.rs`, `docs/proxy/CSP.md`.

### Types / APIs

- `RequestContext`
- `RequestKind` (all 6+ variants)
- `ParsedBody`
- `McpMethod` + `needs_response_buffering`
- `parse_body`, `parse_message` (the top-level ones)
- `WidgetSource`
- Config keys: `widgets`, `publicWidgetDomain`
- Widget static-serving tests in `run.rs`, `route.rs`, `widgets.rs`

Kept: `CspConfig`, `DirectivePolicy`, `Mode`, `WidgetScoped`, `effective_domains`, `RewriteConfig`, `rewrite_response`, `has_markers`, `rewrite_in_place`, `[csp]` and `[[csp.widget]]` config sections.

---

## Risk callouts

- **Event-emission continuity.** The TUI and cloud dashboard consume `RequestEvent` on the existing event bus. The new `Emit` stage must produce events with the same fields in the same shape — otherwise downstream subscribers break. Add a compatibility assertion in Phase 5 (snapshot test of `RequestEvent` structure) before deleting the old `emit.rs`.
- **Session id propagation.** `mcp-session-id` header capture currently happens inside `handlers/mod.rs::capture_session_id`, called from every buffered / streamed path. In the new shape, the `Transport` layer captures it from upstream responses and writes it into the `Response` variant's headers; `SessionRecordMiddleware` reads from there. Verify the capture point in Phase 4 before Phase 5 cutover.
- **Phase 5 is inherently large.** The atomic cutover touches 15+ files. Splitting it into smaller PRs requires keeping both engines wired in parallel, which contradicts the "no feature flags" principle. Accept the large PR; compensate with extra reviewer attention and a live smoke test before merge.
- **`mcpr-cloud/` coupling.** The cloud backend consumes `RequestEvent` and session-lifecycle events. Check `mcpr-cloud/backend/src/` for assumptions about event payload shape before Phase 5. If cloud code pattern-matches on removed fields (`tags: Vec<&str>` stringly-typed flags, `tool: Option<String>`), either update cloud in lockstep or keep the event shape byte-compatible across the cutover.
- **Test coverage during migration.** Phases 2–4 add types and middlewares with unit tests, but end-to-end behavior is only validated in Phase 5. Consider writing an integration test in Phase 2 that exercises a full flow against the *new* pipeline (with a stub transport) so Phase 5 isn't the first time the new engine sees a full request.
- **Server-push observability gap.** The legacy SSE path stays a byte passthrough post-refactor. Server→client notifications and server-initiated requests flow through without the proxy observing them. Accept this for v1; the `ServerPushBroker` refactor is a separate track.
- **Session subsystem stays as-is.** `MemorySessionStore` keeps its async `SessionStore` trait, 4-variant state enum, and scattered event emission across handlers. Middlewares live with this shape for now — a later session refactor tightens it.

---

## Sequencing summary

```
Phase 1 ─ Widget purge         ─ independent, any time
Phase 2 ─ Type foundation      ─ independent, any time
Phase 3 ─ Middlewares          ─ after Phase 2
Phase 4 ─ Router + Transport   ─ after Phase 2 (parallel to 3 if careful)
Phase 5 ─ Atomic cutover       ─ after Phase 3 AND Phase 4
Phase 6 ─ Post-cutover polish  ─ after Phase 5
```

Phase 1 can ship today; it's purely deletion of a decided non-feature. Phases 3 and 4 can run in parallel if two people are working. Phase 5 is the gate — nothing after it until it's in.

Deferred tracks (not part of this refactor):
- **Session subsystem refactor** — rename `MemorySessionStore` → `SessionManager`, collapse state enum, centralize event emission, add idle reaper. Independent of the pipeline rewrite.
- **Server-push plumbing** — `ServerPushBroker` + `ServerPushSubscribe`/`ServerPushPublish` middlewares. New feature; design separately.
