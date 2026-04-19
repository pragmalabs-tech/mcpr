#!/usr/bin/env bash
# perf/realistic-mix.sh — production method proportions, per-method timing.
#
# Each worker runs a deterministic cycle over:
#   ~63% tools/call
#   ~11% tools/list
#   ~11% resources/list
#    ~6% resources/templates/list
#    ~6% resources/read
#
# Plus one implicit `initialize` per session open. Reports per-method
# percentiles under proxied load. For a direct baseline, invoke
# session-bench with --url pointing at the mock port.

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
. "$SCRIPT_DIR/../../lib.sh"

CONNECTIONS="${CONNECTIONS:-1}"
DURATION="${DURATION:-5s}"
WARMUP="${WARMUP:-1s}"

require_tools
build_bins --bin stateful-mock --bin session-bench
trap teardown EXIT INT TERM

start_mock stateful
start_proxy

BENCH_BIN="$BENCH_DIR/target/release/session-bench"
OUT="$RESULTS_DIR/$(now)-realistic-mix.txt"

{
    perf_header "realistic-mix" \
        "rmcp client · $CONNECTIONS sessions · ${DURATION} · deterministic method cycle · proxied"

    echo
    "$BENCH_BIN" --url "http://127.0.0.1:${PROXY_PORT}/mcp" \
        --workload realistic-mix \
        -c "$CONNECTIONS" -z "$DURATION" --warmup "$WARMUP"
} | tee "$OUT"

echo
echo "result saved: $OUT"
