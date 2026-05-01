# mcpr bench harness

Standalone crate (not a workspace member) for measuring mcpr's overhead
on a realistic MCP flow: open a session (`initialize`), then loop
`tools/call get_weather` against the weather example.

**Developer-run.** Numbers are directional, not CI gates.

## Quick start

```bash
# Build mcpr release binary and export MCPR_BIN.
eval "$(scripts/use-local.sh)"

# Run the bench. Starts the weather-app upstream, mcpr proxy, runs
# session-bench direct then proxied.
scripts/bench-weather.sh
```

Results land in `results/` (gitignored). Commit a snapshot in `reports/`
if the numbers are worth keeping.

## What it measures

One real rmcp session per worker:

1. `initialize` (handled automatically by `rmcp::ServiceExt::serve`).
2. Loop `tools/call get_weather { "city": "Tokyo" }` until the deadline.

Per-call latency samples are taken after the warmup window. The script
runs the loop twice: once against the weather-app directly
(`:9001/mcp`), once through mcpr (`:9100/mcp`). Both reports are written
to `results/`.

## Knobs

| Var           | Default              | Notes                                |
|---------------|----------------------|--------------------------------------|
| `MCPR_BIN`    | `mcpr` (PATH)        | Set to `target/release/mcpr` for dev |
| `CONNECTIONS` | `1`                  | Concurrent rmcp sessions             |
| `DURATION`    | `10s`                | Measurement window per side          |
| `WARMUP`      | `2s`                 | Skipped before sampling              |
| `TOOL`        | `get_weather`        | Tool name to invoke                  |
| `ARGS`        | `{"city":"Tokyo"}`   | JSON arguments object                |
| `UPSTREAM_PORT` | `9001`             | Weather-app port                     |
| `PROXY_PORT`  | `9100`               | mcpr port                            |

## Rules for honest numbers

1. Quote µs deltas, not %. Percentages depend on upstream speed.
2. Always pair direct and proxied. A proxied number alone is meaningless.
3. Report p50 / p95 / p99. Means hide everything interesting.
4. Use the warmup window. First few calls are cold paths.
5. Methodology block in any committed report (hardware, OS, mcpr commit).
6. Loopback only. Don't mix proxy overhead with network RTT.

## Layout

```
src/bin/session_bench.rs   rmcp client: initialize + tools/call loop
scripts/bench-weather.sh   single bench scenario (direct vs proxied)
scripts/lib.sh             shared start/wait/teardown plumbing
scripts/use-local.sh       cargo build + export MCPR_BIN
configs/bench.toml         minimal mcpr config, points at weather-app
reports/                   committed reference runs
results/                   per-run output (gitignored)
```

The upstream is the actual `examples/weather-app` server. `bench-weather.sh`
runs `npm install` once and `npm start` automatically.
