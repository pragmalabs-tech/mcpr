#!/usr/bin/env bash
# bench-weather.sh — initialize + tools/call get_weather, direct vs proxied.
#
# Spins up the weather-app example as the upstream, runs mcpr in front of
# it, then runs session-bench against both. Reports each side's
# percentiles. The delta between the two is mcpr's overhead on the
# common-path tool call.

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
. "$SCRIPT_DIR/lib.sh"

CONNECTIONS="${CONNECTIONS:-1}"
DURATION="${DURATION:-10s}"
WARMUP="${WARMUP:-2s}"
TOOL="${TOOL:-get_weather}"
ARGS="${ARGS:-{\"city\":\"Tokyo\"}}"

require_tools curl npm
build_session_bench
trap teardown EXIT INT TERM

start_weather_app
start_proxy

BENCH_BIN="$BENCH_DIR/target/release/session-bench"
OUT="$RESULTS_DIR/$(now)-bench-weather.txt"

{
    echo "==== bench-weather: initialize + tools/call $TOOL ===="

    bench_header "direct" \
        "http://127.0.0.1:${UPSTREAM_PORT}/mcp" \
        "$CONNECTIONS" "$DURATION" "$WARMUP"
    "$BENCH_BIN" \
        --url "http://127.0.0.1:${UPSTREAM_PORT}/mcp" \
        --tool "$TOOL" --args "$ARGS" \
        -c "$CONNECTIONS" -z "$DURATION" --warmup "$WARMUP" \
        --label direct

    echo
    bench_header "proxied" \
        "http://127.0.0.1:${PROXY_PORT}/mcp" \
        "$CONNECTIONS" "$DURATION" "$WARMUP"
    "$BENCH_BIN" \
        --url "http://127.0.0.1:${PROXY_PORT}/mcp" \
        --tool "$TOOL" --args "$ARGS" \
        -c "$CONNECTIONS" -z "$DURATION" --warmup "$WARMUP" \
        --label proxied
} | tee "$OUT"

echo
echo "result saved: $OUT"
