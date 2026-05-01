#!/usr/bin/env bash
# Shared plumbing sourced by the bench script. Keeps one source of truth
# for starting the upstream + proxy, readiness probes, and teardown.

set -euo pipefail

UPSTREAM_PORT="${UPSTREAM_PORT:-9001}"
PROXY_PORT="${PROXY_PORT:-9100}"
PROXY_NAME="${PROXY_NAME:-bench}"

# mcpr binary. Override with MCPR_BIN to bench a local cargo build.
#   eval "$(scripts/use-local.sh)"
MCPR="${MCPR_BIN:-mcpr}"

BENCH_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
REPO_DIR="$(cd "$BENCH_DIR/.." && pwd)"
WEATHER_APP_DIR="$REPO_DIR/examples/weather-app"
RESULTS_DIR="$BENCH_DIR/results"
mkdir -p "$RESULTS_DIR"

WEATHER_PID=""
PROXY_PID=""

require_tools() {
    if ! command -v "$MCPR" >/dev/null 2>&1 && ! [[ -x "$MCPR" ]]; then
        echo "missing mcpr: '$MCPR' (override with MCPR_BIN=/path/to/mcpr)" >&2
        exit 2
    fi
    for t in "$@"; do
        command -v "$t" >/dev/null || { echo "missing $t on PATH" >&2; exit 2; }
    done
}

build_session_bench() {
    (cd "$BENCH_DIR" && cargo build --release --bin session-bench >/dev/null)
}

start_weather_app() {
    [[ -d "$WEATHER_APP_DIR" ]] || {
        echo "missing weather example at $WEATHER_APP_DIR" >&2
        exit 2
    }
    if [[ ! -d "$WEATHER_APP_DIR/node_modules" ]]; then
        echo "==> npm install (weather-app)"
        (cd "$WEATHER_APP_DIR" && npm install --silent >/dev/null)
    fi
    local tsx="$WEATHER_APP_DIR/node_modules/.bin/tsx"
    [[ -x "$tsx" ]] || { echo "missing $tsx (run npm install in weather-app)" >&2; exit 2; }
    echo "==> starting weather-app on :$UPSTREAM_PORT"
    # `exec` replaces the subshell with tsx so WEATHER_PID is the server
    # itself, not an npm wrapper. Otherwise teardown leaves tsx orphaned
    # and the next run hits EADDRINUSE.
    (cd "$WEATHER_APP_DIR" && PORT="$UPSTREAM_PORT" exec "$tsx" server.ts) \
        >/tmp/mcpr-bench-weather.log 2>&1 &
    WEATHER_PID=$!
    wait_http "http://127.0.0.1:${UPSTREAM_PORT}/health"
}

start_proxy() {
    "$MCPR" proxy stop "$PROXY_NAME" >/dev/null 2>&1 || true
    echo "==> starting mcpr proxy '$PROXY_NAME' on :$PROXY_PORT  (binary: $MCPR)"
    "$MCPR" proxy run "$BENCH_DIR/configs/bench.toml" \
        >/tmp/mcpr-bench-proxy.log 2>&1 &
    PROXY_PID=$!
    wait_proxy
}

wait_http() {
    local url="$1" deadline=$(( $(date +%s) + 15 ))
    until curl -fsS "$url" >/dev/null 2>&1; do
        (( $(date +%s) < deadline )) || { echo "timeout waiting for $url" >&2; exit 1; }
        sleep 0.2
    done
}

wait_proxy() {
    local deadline=$(( $(date +%s) + 15 ))
    until curl -sS -o /dev/null -X POST "http://127.0.0.1:${PROXY_PORT}/mcp" \
            -H "content-type: application/json" \
            -H "accept: application/json, text/event-stream" \
            -d '{"jsonrpc":"2.0","id":0,"method":"ping"}' 2>/dev/null; do
        (( $(date +%s) < deadline )) || { echo "proxy not answering on :${PROXY_PORT}" >&2; exit 1; }
        sleep 0.2
    done
}

teardown() {
    [[ -n "$PROXY_PID" ]] && kill "$PROXY_PID" 2>/dev/null || true
    "$MCPR" proxy stop "$PROXY_NAME" >/dev/null 2>&1 || true
    [[ -n "$WEATHER_PID" ]] && kill "$WEATHER_PID" 2>/dev/null || true
    wait 2>/dev/null || true
}

now() { date +%Y%m%dT%H%M%S; }

bench_header() {
    local label="$1" url="$2" conns="$3" duration="$4" warmup="$5"
    local hw mcpr_ver
    hw=$(uname -sm 2>/dev/null || echo "unknown")
    mcpr_ver=$("$MCPR" --version 2>&1 | head -1)
    echo "────────────────────────────────────────────────────────────────"
    printf '  %-12s %s\n' "label:"     "$label"
    printf '  %-12s %s\n' "url:"       "$url"
    printf '  %-12s %s\n' "conns:"     "$conns"
    printf '  %-12s %s\n' "warmup:"    "$warmup"
    printf '  %-12s %s\n' "duration:"  "$duration"
    printf '  %-12s %s\n' "hardware:"  "$hw"
    printf '  %-12s %s\n' "mcpr:"      "$mcpr_ver"
    printf '  %-12s %s\n' "date:"      "$(date +%Y-%m-%dT%H:%M:%S)"
    echo "────────────────────────────────────────────────────────────────"
}
