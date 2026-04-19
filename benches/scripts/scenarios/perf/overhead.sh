#!/usr/bin/env bash
# perf/overhead.sh — baseline per-request proxy overhead.
#
# One connection, zero-latency stateless upstream, `tools/call echo`.
# Minimizes concurrency noise so deltas reflect pure proxy cost per
# request, not queueing. Reports median-of-N direct vs proxied.
#
# Use this to answer: "how many µs does mcpr add on the common path?"
# Ceiling value — realistic upstreams push the percentage to noise.

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
. "$SCRIPT_DIR/../../lib.sh"

RUNS="${RUNS:-5}"
DURATION="${DURATION:-10s}"
CONNECTIONS="${CONNECTIONS:-1}"

require_tools oha
build_bins --bin stateless-mock
trap teardown EXIT INT TERM

start_mock stateless
start_proxy

BODY=$(cat "$BENCH_DIR/payloads/tools_call.json")
OUT="$RESULTS_DIR/$(now)-overhead.txt"

{
    perf_header "overhead" \
        "tools/call echo · $CONNECTIONS conn · ${DURATION}/run · $RUNS runs · median reported"

    # Warmup against proxy so both paths start with warm TCP state.
    oha_run "http://127.0.0.1:${PROXY_PORT}/mcp" "3s" "$CONNECTIONS" "$BODY" >/dev/null
    oha_run "http://127.0.0.1:${MOCK_PORT}/mcp" "3s" "$CONNECTIONS" "$BODY" >/dev/null

    echo
    echo "── measuring direct ($RUNS × $DURATION) ──"
    oha_median "$RUNS" "http://127.0.0.1:${MOCK_PORT}/mcp" "$DURATION" "$CONNECTIONS" "$BODY"
    DIRECT_P50="$M_P50"; DIRECT_P95="$M_P95"; DIRECT_P99="$M_P99"; DIRECT_RPS="$M_RPS"

    echo
    echo "── measuring proxied ($RUNS × $DURATION) ──"
    oha_median "$RUNS" "http://127.0.0.1:${PROXY_PORT}/mcp" "$DURATION" "$CONNECTIONS" "$BODY"
    PROXY_P50="$M_P50"; PROXY_P95="$M_P95"; PROXY_P99="$M_P99"; PROXY_RPS="$M_RPS"

    render_compare_table \
        "$DIRECT_P50" "$DIRECT_P95" "$DIRECT_P99" "$DIRECT_RPS" \
        "$PROXY_P50" "$PROXY_P95" "$PROXY_P99" "$PROXY_RPS"
} | tee "$OUT"

echo
echo "result saved: $OUT"
