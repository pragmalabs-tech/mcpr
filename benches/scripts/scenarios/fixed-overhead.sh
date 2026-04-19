#!/usr/bin/env bash
# Fixed proxy overhead — stateless upstream, zero latency.
# Answers: "how many µs does mcpr add on plain forwarding?"
#
# Uses the stateless mock so every oha request can hit `tools/call echo`
# directly (no session handshake). Upstream latency = 0 means all observed
# latency is either network/kernel or mcpr itself.

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
. "$SCRIPT_DIR/../lib.sh"

DURATION="${DURATION:-10s}"
CONNECTIONS="${CONNECTIONS:-20}"

require_tools oha
build_bins --bin stateless-mock
trap teardown EXIT INT TERM

start_mock stateless
start_proxy

BODY=$(cat "$BENCH_DIR/payloads/tools_call.json")
OUT="$RESULTS_DIR/$(now)-fixed-overhead-c${CONNECTIONS}.txt"

{
    echo "scenario:    fixed-overhead"
    echo "duration:    $DURATION"
    echo "connections: $CONNECTIONS"
    echo "mock:        stateless (latency=0µs)"
    echo "payload:     tools/call echo"
    echo "mcpr:        $(mcpr --version 2>&1 | head -1)"
    echo

    echo "==> warmup (3s)"
    oha_run "http://127.0.0.1:${PROXY_PORT}/mcp" "3s" "$CONNECTIONS" "$BODY" >/dev/null
    echo

    echo "==> baseline (direct to stateless-mock)"
    oha_run "http://127.0.0.1:${MOCK_PORT}/mcp" "$DURATION" "$CONNECTIONS" "$BODY"
    echo

    echo "==> proxied (through mcpr)"
    oha_run "http://127.0.0.1:${PROXY_PORT}/mcp" "$DURATION" "$CONNECTIONS" "$BODY"
    echo
} | tee "$OUT"

echo
echo "result saved: $OUT"
