#!/usr/bin/env bash
# perf/realistic-latency.sh — sweep upstream latency, show that overhead
# collapses to noise as upstream gets slower.
#
# At 0 ms upstream the proxy overhead % looks bad (upstream doing
# nothing makes mcpr's fixed cost dominate). At 10 ms upstream the
# overhead is <5%. At 100 ms it's <1%. This scenario shows the full
# curve so operators can look up their own upstream speed.

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
. "$SCRIPT_DIR/../../lib.sh"

RUNS="${RUNS:-3}"
DURATION="${DURATION:-6s}"
CONNECTIONS="${CONNECTIONS:-10}"
LATENCIES="${LATENCIES:-0 1000 10000 100000}"   # µs

require_tools oha
build_bins --bin stateless-mock
trap teardown EXIT INT TERM

BODY=$(cat "$BENCH_DIR/payloads/tools_call.json")
OUT="$RESULTS_DIR/$(now)-realistic-latency.txt"

{
    perf_header "realistic-latency" \
        "tools/call · $CONNECTIONS conns · ${DURATION}/run · $RUNS runs · upstream latencies: $LATENCIES µs"

    printf '\n  %-10s %14s %14s %14s %14s %12s\n' \
        'upstream' 'direct p50 µs' 'proxied p50 µs' 'Δ p50 µs' 'Δ p99 µs' 'rps loss'
    printf '  %s\n' '──────────────────────────────────────────────────────────────────────────────'

    for lat_us in $LATENCIES; do
        teardown 2>/dev/null || true
        sleep 0.5
        start_mock stateless --latency-us "$lat_us"
        start_proxy
        oha_run "http://127.0.0.1:${MOCK_PORT}/mcp" "2s" "$CONNECTIONS" "$BODY" >/dev/null
        oha_run "http://127.0.0.1:${PROXY_PORT}/mcp" "2s" "$CONNECTIONS" "$BODY" >/dev/null

        oha_median "$RUNS" "http://127.0.0.1:${MOCK_PORT}/mcp" "$DURATION" "$CONNECTIONS" "$BODY"
        local_dp50="$M_P50"; local_dp99="$M_P99"; local_drps="$M_RPS"
        oha_median "$RUNS" "http://127.0.0.1:${PROXY_PORT}/mcp" "$DURATION" "$CONNECTIONS" "$BODY"
        local_pp50="$M_P50"; local_pp99="$M_P99"; local_prps="$M_RPS"

        delta_p50=$((local_pp50 - local_dp50))
        delta_p99=$((local_pp99 - local_dp99))
        loss_pct=$(awk -v d="$local_drps" -v p="$local_prps" \
            'BEGIN{if (d == 0) print "—"; else printf "%+.1f%%", (p - d) / d * 100}')

        lat_label=$(awk -v us="$lat_us" 'BEGIN{
            if (us == 0) print "0 µs";
            else if (us < 1000) printf "%d µs", us;
            else if (us < 1000000) printf "%d ms", us / 1000;
            else printf "%.1f s", us / 1000000
        }')
        printf '  %-10s %14d %14d %+14d %+14d %12s\n' \
            "$lat_label" "$local_dp50" "$local_pp50" "$delta_p50" "$delta_p99" "$loss_pct"
    done
} | tee "$OUT"

echo
echo "result saved: $OUT"
