# mcpr bench harness

Standalone crate (not a workspace member) for measuring mcpr's proxy overhead
and exercising protocol behaviors that unit tests don't cover.

## Quick start

```bash
# Install the external load tool once.
cargo install oha

# Run a single scenario (from benches/).
scripts/scenarios/fixed-overhead.sh      # absolute proxy overhead vs direct
scripts/scenarios/session-churn.sh       # initialize-only worst case
scripts/scenarios/sse-compat.sh          # SSE framing compatibility check
scripts/scenarios/realistic-overhead.sh  # sweep upstream latency (0/1/10/100 ms)

# Or run the full reference matrix (~3 min).
scripts/all.sh
```

Each scenario prints to stdout and appends a timestamped file under `results/`
(gitignored). When you want a reference number, copy the important parts into
`reports/<version>-<scenario>.md` and commit.

### Benching your local `cargo build` instead of the installed mcpr

By default the scripts call `mcpr` from `$PATH` (the installed release). To
bench your working-tree build, set `MCPR_BIN`:

```bash
# One-shot: build + export in one step, then run anything you want.
eval "$(scripts/use-local.sh)"
scripts/scenarios/fixed-overhead.sh

# Or manually:
cargo build --release -p mcpr-proxy --bin mcpr    # from the repo root
MCPR_BIN=../target/release/mcpr scripts/scenarios/fixed-overhead.sh
```

Local and installed binaries share `~/.mcpr/` (state + sqlite store + proxy
registry), so they can't both run a proxy named `bench` at the same time —
the scripts auto-stop any existing one on start. If you want full isolation,
run the installed `mcprd` daemon on a different port or temporarily stop it.

## What's in here

| File                           | Role                                                           |
|--------------------------------|----------------------------------------------------------------|
| `src/bin/stateful-mock.rs`     | rmcp-backed MCP server — real protocol, real session handling  |
| `src/bin/stateless-mock.rs`    | Hand-rolled JSON-RPC — no session state, `--latency-us` knob   |
| `src/bin/session-bench.rs`     | rmcp-client-backed load tool (one session, many tools/call)    |
| `scripts/scenarios/*.sh`       | One scenario per file — all invoke mcpr + a mock + load tool   |
| `scripts/all.sh`               | Runs every scenario sequentially                               |
| `configs/bench.toml`           | Minimal mcpr config pointing at the mock                       |
| `payloads/*.json`              | JSON-RPC request bodies for oha                                |
| `reports/TEMPLATE.md`          | Copy-and-fill template for reference reports                   |
| `reports/<version>-*.md`       | Committed reference runs                                       |
| `results/`                     | Per-run stdout dumps (gitignored)                              |

## Scenarios — what each measures

| Scenario             | Mock       | Load tool     | Question answered                                   |
|----------------------|------------|---------------|------------------------------------------------------|
| `fixed-overhead`     | stateless  | oha           | How many µs does mcpr add on plain forwarding?       |
| `realistic-overhead` | stateless  | oha           | How does overhead % shrink against a slow upstream?  |
| `session-churn`      | stateful   | oha           | Worst case: one session created per request          |
| `session-reuse`      | stateful   | session-bench | Steady state: one session, many tool calls           |
| `sse-compat`         | stateful   | curl          | Does mcpr byte-pass SSE responses correctly?         |

## Rules for honest numbers

1. **Always quote µs deltas, not %.** Percentages depend on upstream speed; absolute overhead is the stable metric.
2. **Always pair direct and proxied.** A proxied number alone is meaningless.
3. **Report p50 / p95 / p99 / p99.9.** Means hide everything interesting.
4. **Warmup 5 s, measure 30 s minimum, run 3 times.** Single runs are noise.
5. **Methodology block mandatory in every committed report** — CPU, OS, mcpr version, commit SHA. Without it the number is unfalsifiable.
6. **Loopback only.** Don't publish numbers that mix proxy overhead with network RTT.

## Known gaps

- **Mixed workload (init + steady-state + SSE concurrently) not covered.** Each scenario runs in isolation.
- **No CPU/RSS counters.** Throughput/latency only; if you need them, wrap the proxy in `/usr/bin/time -v`.
- **Single-run tail variance is still ±30–50 %** on loopback. `oha_run_multi` in `lib.sh` supports multi-run median reporting; use it for anything claimed as a reference number.

## Previously-known mcpr issues, now resolved

All three issues below were surfaced by this harness and fixed in the
branch-by-shape pipeline refactor (see `docs/proxy/REFACTOR_PLAN.md`).
Scenarios now PASS:

- **SSE byte-pass** (`scripts/scenarios/sse-compat.sh`) — mcpr now forwards
  upstream `text/event-stream` responses unchanged: `transfer-encoding: chunked`
  preserved, SSE metadata (`id:`, `retry:`, empty `data:` prefix lines) intact,
  JSON payload byte-for-byte identical.
- **Multi-event SSE** (`scripts/scenarios/multi-event-sse.sh`) — multi-event
  responses stream through without the old `extract_json_from_sse` silent-drop.
- **Binary passthrough** (`scripts/scenarios/passthrough-binary.sh`) —
  non-JSON responses stream via `Body::from_stream` instead of going through
  `String::from_utf8_lossy` + `.replace()`.

`session-bench` now works against the new pipeline — rmcp clients complete
the initialize handshake through mcpr.
