#!/usr/bin/env bash
# Shared plumbing sourced by every scenario script.
#
# Keeps one source of truth for building mocks, starting mcpr, readiness
# probes, and teardown. Scenarios stay short and readable.

set -euo pipefail

MOCK_PORT="${MOCK_PORT:-9001}"
PROXY_PORT="${PROXY_PORT:-9100}"
PROXY_NAME="${PROXY_NAME:-bench}"

# mcpr binary — override with MCPR_BIN to bench a local cargo build instead
# of the installed release. Example:
#   MCPR_BIN=./target/release/mcpr scripts/scenarios/fixed-overhead.sh
#   (or source scripts/use-local.sh to build + export in one step)
MCPR="${MCPR_BIN:-mcpr}"

BENCH_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
RESULTS_DIR="$BENCH_DIR/results"
mkdir -p "$RESULTS_DIR"

require_tools() {
    if ! command -v "$MCPR" >/dev/null 2>&1 && ! [[ -x "$MCPR" ]]; then
        echo "missing mcpr: '$MCPR' (override with MCPR_BIN=/path/to/mcpr)" >&2
        exit 2
    fi
    for t in "$@"; do
        command -v "$t" >/dev/null || { echo "missing $t on PATH" >&2; exit 2; }
    done
}

build_bins() {
    (cd "$BENCH_DIR" && cargo build --release "$@" >/dev/null)
}

start_mock() {
    local kind="$1"; shift
    local bin="$BENCH_DIR/target/release/${kind}-mock"
    [[ -x "$bin" ]] || { echo "build first: $bin missing" >&2; exit 2; }
    echo "==> starting ${kind}-mock on :$MOCK_PORT  args=[$*]"
    "$bin" --bind "127.0.0.1:${MOCK_PORT}" "$@" >/tmp/mcpr-bench-mock.log 2>&1 &
    MOCK_PID=$!
    wait_http "http://127.0.0.1:${MOCK_PORT}/healthz"
}

start_proxy() {
    # Clear any leftover proxy with the same name, then start fresh.
    "$MCPR" proxy stop "$PROXY_NAME" >/dev/null 2>&1 || true
    echo "==> starting mcpr proxy '$PROXY_NAME' on :$PROXY_PORT  (binary: $MCPR)"
    "$MCPR" proxy run -c "$BENCH_DIR/configs/bench.toml" >/dev/null
    wait_proxy
}

wait_http() {
    local url="$1" deadline=$(( $(date +%s) + 10 ))
    until curl -fsS "$url" >/dev/null 2>&1; do
        (( $(date +%s) < deadline )) || { echo "timeout waiting for $url" >&2; exit 1; }
        sleep 0.1
    done
}

wait_proxy() {
    local deadline=$(( $(date +%s) + 10 ))
    until curl -sS -o /dev/null -X POST "http://127.0.0.1:${PROXY_PORT}/mcp" \
            -H "content-type: application/json" \
            -H "accept: application/json, text/event-stream" \
            -d '{"jsonrpc":"2.0","id":0,"method":"ping"}' 2>/dev/null; do
        (( $(date +%s) < deadline )) || { echo "proxy not answering on :${PROXY_PORT}" >&2; exit 1; }
        sleep 0.1
    done
}

teardown() {
    "$MCPR" proxy stop "$PROXY_NAME" >/dev/null 2>&1 || true
    [[ -n "${MOCK_PID:-}" ]] && kill "$MOCK_PID" 2>/dev/null || true
    wait 2>/dev/null || true
}

# oha wrapper — sane defaults, same flags everywhere.
oha_run() {
    local url="$1" duration="$2" connections="$3" body="$4"
    oha --no-tui \
        -z "$duration" -c "$connections" \
        -m POST \
        -H "content-type: application/json" \
        -H "accept: application/json, text/event-stream" \
        -d "$body" \
        --latency-correction \
        "$url"
}

# Timestamp for result files.
now() { date +%Y%m%dT%H%M%S; }
