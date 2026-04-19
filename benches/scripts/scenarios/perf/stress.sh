#!/usr/bin/env bash
# perf/stress.sh — concurrency ramp against tool_call.
#
# Answers: "what throughput and p99 does the proxy sustain as
# concurrent clients grow?" Runs at several concurrency levels and
# reports each level's median p50/p95/p99/rps. Reveals where the
# semaphore (100 permits) starts to matter and where tail latency
# knees up.
#
# Kept at stateless upstream so upstream work isn't the bottleneck.

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
. "$SCRIPT_DIR/../../lib.sh"

RUNS="${RUNS:-3}"
DURATION="${DURATION:-8s}"
LEVELS="${LEVELS:-1 10 20 50}"   # loopback port exhaustion caps us below 100 on macOS

require_tools oha
build_bins --bin stateless-mock
trap teardown EXIT INT TERM

start_mock stateless
start_proxy

BODY=$(cat "$BENCH_DIR/payloads/tools_call.json")
OUT="$RESULTS_DIR/$(now)-stress.txt"

{
    perf_header "stress" \
        "tools/call · levels=[$LEVELS] · ${DURATION}/run · $RUNS runs per level"

    printf '\n  %-8s %12s %12s %12s %12s\n' 'conns' 'p50 (µs)' 'p95 (µs)' 'p99 (µs)' 'rps'
    printf '  %s\n' '────────────────────────────────────────────────────────────'

    for conns in $LEVELS; do
        oha_run "http://127.0.0.1:${PROXY_PORT}/mcp" "2s" "$conns" "$BODY" >/dev/null
        oha_median "$RUNS" "http://127.0.0.1:${PROXY_PORT}/mcp" "$DURATION" "$conns" "$BODY"
        printf '  %-8s %12d %12d %12d %12.0f\n' \
            "$conns" "$M_P50" "$M_P95" "$M_P99" "$M_RPS"
    done

    echo
    echo "note: only the proxied path measured — find where p99 knees up."
} | tee "$OUT"

echo
echo "result saved: $OUT"
