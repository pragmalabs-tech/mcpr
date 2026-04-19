# mcpr vX.Y.Z — <scenario name>

<!-- Copy this file to reports/vX.Y.Z-<scenario>.md and fill in. -->

## Methodology

```
Hardware:    <CPU model>, <cores>c, <RAM>GB
OS:          <uname -rs>
Rust:        <rustc --version>
mcpr:        <mcpr --version>  (commit <short sha>)
Load tool:   oha <ver>          (or wrk2 <ver> for CO-corrected tails)
Topology:    client, proxy, mock all on loopback, no CPU pinning
Warmup:      5s
Run length:  30s
Runs:        3, median reported
```

## Config

```toml
# benches/configs/bench.toml (inline for reproducibility)
name = "bench"
mcp = "http://127.0.0.1:9001/mcp"
port = 9100
```

Mock: `stateless-mock` with `--latency-us <N>` (or `stateful-mock` for session flow).

## Results

### Fixed overhead — `tools/call echo` at 100 connections, varying upstream latency

| Upstream | Direct p50 | Proxied p50 | Δ p50 | Δ p99 | Direct rps | Proxied rps | Loss |
|----------|-----------:|------------:|------:|------:|-----------:|------------:|-----:|
| 0 µs     |            |             |       |       |            |             |      |
| 1 ms     |            |             |       |       |            |             |      |
| 10 ms    |            |             |       |       |            |             |      |
| 100 ms   |            |             |       |       |            |             |      |

### Session churn — `initialize` at 100 connections (stateful mock)

| Percentile | Direct | Proxied | Δ       |
|------------|-------:|--------:|--------:|
| p50        |        |         |         |
| p95        |        |         |         |
| p99        |        |         |         |

### Session reuse — `tools/call echo` over one session (session-bench)

| Connections | p50 | p95 | p99 | req/s |
|-------------|----:|----:|----:|------:|
| 1           |     |     |     |       |
| 10          |     |     |     |       |
| 100         |     |     |     |       |

### Proxy-internal view (`mcpr proxy status`)

| Tool          | Calls | Avg    | p95    | Max    |
|---------------|------:|-------:|-------:|-------:|
| echo          |       |        |        |        |
| `<initialize>`|       |        |        |        |

## Interpretation

<!-- Summarize the story the numbers tell. Two or three bullets. Quote
absolute µs deltas, not percentages. Call out anything surprising. -->

-
-

## Caveats

<!-- Known limitations of this run. Loopback? Single-host? Payload sizes? -->

-
