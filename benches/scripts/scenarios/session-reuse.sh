#!/usr/bin/env bash
# Session reuse — stateful upstream, one rmcp session per worker, many tools/call.
# Answers: "what's mcpr's overhead for steady-state tool invocation?"
#
# STATUS: currently blocked by the SSE forwarding bug (benches/README.md
# § Known mcpr issues). The rmcp client parser rejects mcpr's reformatted
# SSE frames during initialize. Keep this script in place — flip once
# the SSE byte-passing fix lands.

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
. "$SCRIPT_DIR/../lib.sh"

DURATION="${DURATION:-10s}"
CONNECTIONS="${CONNECTIONS:-10}"

require_tools
build_bins --bin stateful-mock --bin session-bench
trap teardown EXIT INT TERM

start_mock stateful
start_proxy

OUT="$RESULTS_DIR/$(now)-session-reuse-c${CONNECTIONS}.txt"

{
    echo "scenario:    session-reuse"
    echo "duration:    $DURATION"
    echo "connections: $CONNECTIONS"
    echo "mock:        stateful (rmcp)"
    echo "load tool:   session-bench (rmcp client — one session per worker)"
    echo "mcpr:        $(mcpr --version 2>&1 | head -1)"
    echo

    echo "==> baseline (session-bench → stateful-mock directly)"
    "$BENCH_DIR/target/release/session-bench" \
        --url "http://127.0.0.1:${MOCK_PORT}/mcp" \
        -c "$CONNECTIONS" -z "$DURATION" --warmup 2s
    echo

    echo "==> proxied (session-bench → mcpr → stateful-mock)"
    "$BENCH_DIR/target/release/session-bench" \
        --url "http://127.0.0.1:${PROXY_PORT}/mcp" \
        -c "$CONNECTIONS" -z "$DURATION" --warmup 2s
    echo
} | tee "$OUT"

echo
echo "result saved: $OUT"
