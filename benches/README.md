# mcpr bench harness

Standalone crate (not a workspace member) for measuring mcpr's proxy overhead,
exercising protocol behaviors that unit tests don't cover, and diagnosing
where time goes in the request pipeline.

**Developer-run.** Scenarios are designed to be executed locally — they
produce directional numbers, not CI-gate-worthy SLOs. Correctness
scenarios are deterministic and safe to wire into CI when needed.

## Quick start

```bash
# Install the external load tool once.
cargo install oha

# Build + export MCPR_BIN for the local working-tree binary.
eval "$(scripts/use-local.sh)"

# Run the full suite (correctness gates + perf scenarios).
scripts/all.sh

# Or run a single scenario.
scripts/scenarios/perf/overhead.sh
scripts/scenarios/correctness/sse-compat.sh
scripts/scenarios/diagnostics/where-time-goes.sh
```

Results land in `results/` (gitignored). Commit meaningful numbers as a
dated report in `reports/`.

## Scenario map

```
scripts/scenarios/
├── correctness/                ← deterministic, CI-gateable
│   ├── sse-compat.sh
│   ├── multi-event-sse.sh
│   └── passthrough-binary.sh
├── perf/                       ← directional, dev-run
│   ├── overhead.sh
│   ├── stress.sh
│   ├── session-reuse.sh
│   ├── realistic-mix.sh
│   └── realistic-latency.sh
└── diagnostics/
    └── where-time-goes.sh
```

### Correctness

| Scenario              | Asserts                                              |
|-----------------------|------------------------------------------------------|
| `sse-compat.sh`       | SSE body byte-match + `transfer-encoding: chunked`   |
| `multi-event-sse.sh`  | Multi-event SSE framing preserved                    |
| `passthrough-binary.sh` | Non-JSON binary byte-pass                           |

Deterministic exit codes: `PASS → 0`, `FAIL → 1`. Can be gated in CI.

### Perf

All perf scenarios follow the same format: methodology header →
direct vs proxied (when applicable) → median across N runs → delta table.

| Scenario              | Workload                   | Answers                                               |
|-----------------------|----------------------------|-------------------------------------------------------|
| `overhead.sh`         | 1 conn, 0 ms, tools/call   | Floor per-request overhead (µs added)                 |
| `stress.sh`           | Ramp 1/10/20/50 conns      | Peak throughput ceiling + where p99 knees up          |
| `session-reuse.sh`    | Real rmcp client, tools/call loop | Steady-state latency seen by a real client     |
| `realistic-mix.sh`    | Production method cycle (63% tools/call, 16% tools/list, …) | Per-method percentiles under realistic load |
| `realistic-latency.sh`| Sweep upstream 0/1/10/100 ms | How much overhead % shrinks as upstream slows       |

### Diagnostics

- `where-time-goes.sh` — per-stage timing breakdown via `MCPR_STAGE_TIMING=1`.
  Sets the env var automatically. Aggregates `stage_timings` from the proxy
  log and prints median/p95/max per stage. Run this **after** a perf scenario
  shows a regression, to see which stage got slower.

## Knobs

Most scenarios honor these env vars:

| Var              | Default     | Notes                                              |
|------------------|-------------|----------------------------------------------------|
| `MCPR_BIN`       | `mcpr` (PATH) | Set to `target/release/mcpr` for local dev builds |
| `DURATION`       | 6–10 s      | Per-run oha duration                               |
| `CONNECTIONS`    | 1 / 10 / 20 | Scenario-dependent                                 |
| `RUNS`           | 3–5         | Multi-run median aggregation                       |
| `LATENCIES`      | (see scenario) | µs values for `realistic-latency.sh`           |
| `LEVELS`         | (see scenario) | concurrency values for `stress.sh`             |
| `SKIP_STRESS`    | 0           | Skip `stress.sh` in `all.sh`                       |
| `SKIP_LATENCY`   | 0           | Skip `realistic-latency.sh` in `all.sh`            |

## Rules for honest numbers

1. **Quote µs deltas, not %.** Percentages depend on upstream speed; absolute overhead is the stable metric.
2. **Always pair direct and proxied** — a proxied number alone is meaningless.
3. **Report p50 / p95 / p99** — means hide everything interesting.
4. **Warmup + multi-run median** — single runs are noise (±30-50% on loopback).
5. **Methodology block mandatory** in every committed report (hardware, OS, mcpr version, commit SHA).
6. **Loopback only** — don't mix proxy overhead with network RTT.

## Artifacts

- `src/bin/stateful-mock.rs` — rmcp-backed MCP server, real protocol + sessions
- `src/bin/stateless-mock.rs` — hand-rolled JSON-RPC, `--latency-us` knob, `/binary` for passthrough tests
- `src/bin/multi-event-mock.rs` — deterministic multi-event SSE response
- `src/bin/session-bench.rs` — rmcp-client loader with `--workload echo|realistic-mix`
- `configs/bench.toml` — minimal mcpr config pointing at the mock
- `payloads/*.json` — JSON-RPC bodies for oha
- `reports/*.md` — committed reference runs
- `results/` — per-run stdout (gitignored)

## Benching your local `cargo build`

```bash
eval "$(scripts/use-local.sh)"     # builds mcpr + exports MCPR_BIN
scripts/scenarios/perf/overhead.sh
```

Without `MCPR_BIN` the scripts call the installed `mcpr` from `$PATH`. Local
and installed binaries share `~/.mcpr/` (sqlite store + proxy registry) —
they can't both run a proxy named `bench` at the same time, but the scripts
auto-stop any existing one on start.

## Historical findings

Three scenarios in this harness originally caught mcpr bugs that the
unit tests missed:

- **SSE byte-pass** (sse-compat) — upstream `text/event-stream` responses
  are now forwarded unchanged; `transfer-encoding: chunked` preserved,
  SSE metadata intact, JSON byte-for-byte identical.
- **Multi-event SSE** — multi-event responses stream through correctly
  (old code silently dropped framing when more than one `data:` line was
  present).
- **Binary passthrough** — non-JSON responses stream via `Body::from_stream`
  instead of going through `String::from_utf8_lossy` + `.replace()`.

All three are fixed in the branch-by-shape pipeline refactor (see
`docs/proxy/REFACTOR_PLAN.md`). Scenarios remain as regression guards.
