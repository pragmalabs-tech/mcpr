#!/usr/bin/env bash
# Session churn — stateful upstream, every request is `initialize`.
# Answers: "what's mcpr's overhead when sessions are created per request?"
#
# This is the worst case — session middleware fires on every response,
# MemorySessionStore grows, SessionStart events emitted. Real usage is the
# opposite (one init, thousands of reuse). Keep this number for regression
# tracking but don't treat it as an SLO.

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
. "$SCRIPT_DIR/../lib.sh"

DURATION="${DURATION:-10s}"
CONNECTIONS="${CONNECTIONS:-20}"

require_tools oha
build_bins --bin stateful-mock
trap teardown EXIT INT TERM

start_mock stateful
start_proxy

BODY=$(cat "$BENCH_DIR/payloads/initialize.json")
OUT="$RESULTS_DIR/$(now)-session-churn-c${CONNECTIONS}.txt"

{
    echo "scenario:    session-churn"
    echo "duration:    $DURATION"
    echo "connections: $CONNECTIONS"
    echo "mock:        stateful (rmcp, LocalSessionManager)"
    echo "payload:     initialize (creates new session each call)"
    echo "mcpr:        $(mcpr --version 2>&1 | head -1)"
    echo

    echo "==> warmup (3s)"
    oha_run "http://127.0.0.1:${PROXY_PORT}/mcp" "3s" "$CONNECTIONS" "$BODY" >/dev/null
    echo

    echo "==> baseline (direct to stateful-mock)"
    oha_run "http://127.0.0.1:${MOCK_PORT}/mcp" "$DURATION" "$CONNECTIONS" "$BODY"
    echo

    echo "==> proxied (through mcpr)"
    oha_run "http://127.0.0.1:${PROXY_PORT}/mcp" "$DURATION" "$CONNECTIONS" "$BODY"
    echo
} | tee "$OUT"

echo
echo "result saved: $OUT"
