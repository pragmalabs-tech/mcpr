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

## Flow

```
                       bench-weather.sh
                              │
        ┌─────────────────────┼─────────────────────┐
        ▼                     ▼                     ▼
  start_weather_app     start_proxy            session-bench
  (lib.sh)              (lib.sh)               (Rust binary)
        │                     │                     │
        ▼                     ▼                     ▼
   tsx server.ts         mcpr proxy run         reqwest POST loop
   :9001/mcp             :9100 -> :9001         direct, then proxied
```

1. **`start_weather_app`** runs `examples/weather-app/server.ts` via
   `tsx`. The captured PID is the Node server itself (not an `npm`
   wrapper) so teardown kills it cleanly. Probes `/health` until ready.
2. **`start_proxy`** clears any leftover `bench` lock, then runs
   `mcpr proxy run configs/bench.toml` backgrounded. Probes
   `:9100/mcp` with a `ping` POST until the proxy answers.
3. **`session-bench`** is invoked twice: once with `--url :9001/mcp`
   (direct), once with `--url :9100/mcp` (proxied). Per worker:
    - POST `initialize`, capture `mcp-session-id` response header.
    - POST `notifications/initialized` (best-effort, expects `202`).
    - Loop POST `tools/call get_weather { "city": "Tokyo" }` with that
      session ID until the deadline. Record elapsed µs per call.
4. **Teardown** runs on `EXIT`/`INT`/`TERM`: kills mcpr, kills tsx,
   `mcpr proxy stop bench` to clear the lockfile.

The Rust binary is plain `reqwest` over HTTP — no `rmcp` client, no SSE
state machine. That keeps the bench resilient to client-library quirks
and isolates the measurement to wire-level round-trip cost.

## Measurement window

Two timestamps gate each loop:

```
deadline           = now + warmup + duration
measurement_start  = now + warmup
```

Calls before `measurement_start` count toward `total ops` but are
dropped from the latency samples. Defaults: 2 s warmup, 10 s window.

Per side the report is:

```
samples         number of latency samples (post-warmup)
total ops       attempted requests (warmup + window)
errors          non-2xx or transport failures
req/s (window)  samples / window
p50/p95/p99     latency in microseconds, by index over sorted samples
```

## Knobs

| Var             | Default              | Notes                                |
|-----------------|----------------------|--------------------------------------|
| `MCPR_BIN`      | `mcpr` (PATH)        | Set via `eval "$(scripts/use-local.sh)"` for dev builds |
| `CONNECTIONS`   | `1`                  | Concurrent sessions; each opens its own |
| `DURATION`      | `10s`                | Measurement window per side          |
| `WARMUP`        | `2s`                 | Skipped before sampling              |
| `TOOL`          | `get_weather`        | Tool name to invoke                  |
| `ARGS`          | `{"city":"Tokyo"}`   | JSON arguments object                |
| `UPSTREAM_PORT` | `9001`               | Weather-app port                     |
| `PROXY_PORT`    | `9100`               | mcpr port                            |

## Rules for honest numbers

1. Quote µs deltas, not %. Percentages depend on upstream speed.
2. Always pair direct and proxied. A proxied number alone is meaningless.
3. Report p50 / p95 / p99. Means hide everything interesting.
4. Use the warmup window. First few calls are cold paths.
5. Methodology block in any committed report (hardware, OS, mcpr commit).
6. Loopback only. Don't mix proxy overhead with network RTT.

## Layout

```
src/bin/session_bench.rs   reqwest-based HTTP load driver
scripts/bench-weather.sh   single bench scenario (direct vs proxied)
scripts/lib.sh             shared start/wait/teardown plumbing
scripts/use-local.sh       cargo build + export MCPR_BIN
configs/bench.toml         minimal mcpr config, points at weather-app
reports/                   committed reference runs
results/                   per-run output (gitignored)
```

The upstream is the actual `examples/weather-app` server.
`start_weather_app` runs `npm install` once on first invocation, then
launches `tsx server.ts` directly on every run.
