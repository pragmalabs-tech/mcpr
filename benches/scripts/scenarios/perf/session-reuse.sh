#!/usr/bin/env bash
# perf/session-reuse.sh — realistic steady-state tool invocation.
#
# Uses session-bench (real rmcp client) to open one session per
# worker, then loop `tools/call echo` over that session. Matches the
# dominant production pattern (63% tools/call). Measures proxied
# performance — the number a real MCP client sees.
#
# For a direct baseline, invoke session-bench manually:
#   ./target/release/session-bench --url http://127.0.0.1:9001/mcp \
#        -c 3 -z 5s --warmup 1s
# (The rmcp client itself gets unstable under heavy back-to-back
# runs, so this scenario doesn't try to sequence direct + proxied
# in one invocation.)

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
. "$SCRIPT_DIR/../../lib.sh"

CONNECTIONS="${CONNECTIONS:-3}"
DURATION="${DURATION:-5s}"
WARMUP="${WARMUP:-1s}"

require_tools
build_bins --bin stateful-mock --bin session-bench
trap teardown EXIT INT TERM

start_mock stateful
start_proxy

BENCH_BIN="$BENCH_DIR/target/release/session-bench"
OUT="$RESULTS_DIR/$(now)-session-reuse.txt"

{
    perf_header "session-reuse" \
        "rmcp client · $CONNECTIONS sessions · ${DURATION} (+${WARMUP} warmup) · proxied"

    echo
    "$BENCH_BIN" --url "http://127.0.0.1:${PROXY_PORT}/mcp" \
        -c "$CONNECTIONS" -z "$DURATION" --warmup "$WARMUP"
} | tee "$OUT"

echo
echo "result saved: $OUT"
