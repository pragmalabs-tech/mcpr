#!/usr/bin/env bash
# Realistic overhead — stateless upstream, sweep injected upstream latency.
# Answers: "how does mcpr's overhead % shrink against a slow upstream?"
#
# The absolute µs overhead should be roughly constant across latency tiers;
# the percentage should collapse toward zero as upstream gets slower.

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
. "$SCRIPT_DIR/../lib.sh"

DURATION="${DURATION:-10s}"
CONNECTIONS="${CONNECTIONS:-20}"
LATENCIES="${LATENCIES:-0 1000 10000 100000}"   # µs: 0, 1ms, 10ms, 100ms

require_tools oha
build_bins --bin stateless-mock
trap teardown EXIT INT TERM

BODY=$(cat "$BENCH_DIR/payloads/tools_call.json")
OUT="$RESULTS_DIR/$(now)-realistic-overhead-c${CONNECTIONS}.txt"

{
    echo "scenario:    realistic-overhead"
    echo "duration:    $DURATION per latency tier"
    echo "connections: $CONNECTIONS"
    echo "latencies:   $LATENCIES (µs)"
    echo "mcpr:        $(mcpr --version 2>&1 | head -1)"
    echo

    for lat_us in $LATENCIES; do
        teardown 2>/dev/null || true
        sleep 0.3
        start_mock stateless --latency-us "$lat_us"
        start_proxy

        echo "================================================================"
        echo "==> upstream latency = ${lat_us} µs"
        echo "================================================================"

        # Warmup scales with latency — longer warmup at higher latency so
        # connection pools settle before measurement.
        oha_run "http://127.0.0.1:${PROXY_PORT}/mcp" "2s" "$CONNECTIONS" "$BODY" >/dev/null

        echo "-- baseline (direct) --"
        oha_run "http://127.0.0.1:${MOCK_PORT}/mcp" "$DURATION" "$CONNECTIONS" "$BODY"
        echo
        echo "-- proxied (through mcpr) --"
        oha_run "http://127.0.0.1:${PROXY_PORT}/mcp" "$DURATION" "$CONNECTIONS" "$BODY"
        echo
    done
} | tee "$OUT"

echo
echo "result saved: $OUT"
