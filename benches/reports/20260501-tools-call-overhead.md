# tools/call get_weather — mcpr overhead

Real overhead mcpr adds to a `tools/call` round-trip on loopback, measured
direct vs proxied at equal sample counts.

## Summary

| metric  | direct  | proxied | **overhead added** |
|---------|---------|---------|--------------------|
| p50     | 120µs   | 196µs   | **+76µs**          |
| p90     | 145µs   | 238µs   | **+93µs**          |
| p95     | 165µs   | 272µs   | **+107µs**         |
| p99     | 761µs   | 803µs   | **+42µs**          |
| p99.9   | 1026µs  | 1276µs  | **+250µs**         |
| p99.99  | 1598µs  | 2382µs  | **+784µs**         |
| max     | 2023µs  | 5380µs  | **+3357µs**        |

Median: ~76µs per request. Tail grows (~+800µs at p99.99) because the
proxy adds task scheduling and per-frame SSE remux on top of the upstream
RTT.

Throughput on the same run:

| side    | req/s  |
|---------|--------|
| direct  | 7281   |
| proxied | 4638   |

## Where the overhead lives (per-request timer, n ≈ 17k)

From `MCPR_DEBUG=1` timer dump inside `handle_request`:

| span               | avg     | p50     | p95     | notes                                |
|--------------------|---------|---------|---------|--------------------------------------|
| `Parse`            | 828ns   | 792ns   | 1.12µs  | axum body → `Request` enum           |
| `RequestLogStage`  | 1.19µs  | 1.12µs  | 1.67µs  |                                      |
| `Router`           | 124µs   | 111µs   | 148µs   | upstream HTTP call (not "overhead")  |
| `Encode`           | 316ns   | 291ns   | 458ns   | wrap stream Body, no serialize       |

Inside `handle_request` mcpr's measurable cost on top of the upstream
call is **~2µs**. The remaining ~74µs of p50 overhead happens **after**
the handler returns:

- per-frame SSE remux (`sse::encode_one` per frame, response-stage chain
  per frame),
- axum / hyper response body pumping,
- task scheduling between client → handler → upstream → body stream → client.

Response stages (`SessionTracking`, `SchemaTracking`, `CspRewritter`,
`ResponseLog`) only have 2 timer samples each here because every
`tools/call` response is `text/event-stream` and goes through the
`RouterOutput::Stream` path; per-frame stage spans accumulate after the
timer dump fires.

## Reality check

- **+76µs at p50 is small in absolute terms.** Against any real upstream
  (network call, DB, LLM inference) the proxy cost disappears into the
  noise.
- The **40% throughput drop** is a loopback artifact: when the upstream
  itself only takes 120µs, an extra 76µs is comparable work, so RPS halves.
  Against a 10ms upstream the proxy would cost <1% throughput.
- p99 looks artificially small (+42µs) because the direct side already had
  upstream-tail outliers around 760µs; the proxy's own tail only shows
  past p99.9.

## Methodology

- **Hardware:** Darwin arm64 (Apple Silicon)
- **mcpr:** 0.5.0 (release build, `cargo build --release -p mcpr-proxy`)
- **Upstream:** `examples/weather-app` (Node.js + tsx) on loopback
- **Bench:** `session-bench` with `--samples-out` for raw µs samples
- **Window:** 5s measurement, 1s warmup, single connection, single worker
- **Equal-count truncation:** raw samples sliced to `min(direct, proxied)
  = 23,192` before computing percentiles, so tail comparisons are sound
- **Run:** 2026-05-01

## Reproducing

```bash
# Build mcpr release.
cargo build --release -p mcpr-proxy --bin mcpr

# Build session-bench (with --samples-out support).
cd benches && cargo build --release --bin session-bench

# Start upstream + proxy by hand (the bench script doesn't accept
# --samples-out yet).
PORT=9001 examples/weather-app/node_modules/.bin/tsx \
    examples/weather-app/server.ts &
target/release/mcpr proxy run benches/configs/bench.toml &

# Drive both sides, dumping raw samples.
BIN=benches/target/release/session-bench
$BIN --url http://127.0.0.1:9001/mcp --tool get_weather --args '{"city":"Tokyo"}' \
     -c 1 -z 5s --warmup 1s --samples-out /tmp/direct.samples
$BIN --url http://127.0.0.1:9100/mcp --tool get_weather --args '{"city":"Tokyo"}' \
     -c 1 -z 5s --warmup 1s --samples-out /tmp/proxied.samples

# Aggregate at equal sample count (Python).
python3 - <<'PY'
def load(p):
    with open(p) as f: return [int(x) for x in f.read().split()]
d, p = load('/tmp/direct.samples'), load('/tmp/proxied.samples')
n = min(len(d), len(p))
ds, ps = sorted(d[:n]), sorted(p[:n])
def pct(xs, q): return xs[min(len(xs)-1, int(q/100*len(xs)))]
for name, q in [('p50',50),('p90',90),('p95',95),('p99',99),('p99.9',99.9),('p99.99',99.99)]:
    print(f"{name}: direct={pct(ds,q)}µs proxied={pct(ps,q)}µs overhead=+{pct(ps,q)-pct(ds,q)}µs")
PY
```

## Caveats

- **Loopback only.** No network RTT included.
- **Tools/call is SSE.** Response-stage timings under SSE aren't fully
  captured by the `[mcpr] timer` dump (per-frame spans accrue after
  `handle_request` returns).
- **Single connection, single worker.** Multi-connection numbers will
  shift; tail in particular widens with concurrency.
- **Run-to-run variance ~±10µs** at p50, much higher at p99+. Treat
  single numbers as directional, not gates.
