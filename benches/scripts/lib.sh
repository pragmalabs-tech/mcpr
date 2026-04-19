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

# Multi-run oha wrapper. Runs `oha_run` N times, parses each run's
# p50 / p95 / p99 / rps, prints per-run values plus a median summary.
# Median is a more honest number than single-run on noisy loopback.
#
# Usage:
#   oha_run_multi <runs> <url> <duration> <connections> <body>
oha_run_multi() {
    local runs="$1" url="$2" duration="$3" connections="$4" body="$5"
    local tmp
    tmp=$(mktemp)
    local p50s=() p95s=() p99s=() rpss=()

    for i in $(seq 1 "$runs"); do
        echo "  -- run $i/$runs --"
        oha_run "$url" "$duration" "$connections" "$body" | tee "$tmp"
        local p50 p95 p99 rps
        p50=$(awk '/^  50\.00% in/ { print $3; exit }' "$tmp")
        p95=$(awk '/^  95\.00% in/ { print $3; exit }' "$tmp")
        p99=$(awk '/^  99\.00% in/ { print $3; exit }' "$tmp")
        rps=$(awk -F'\t' '/^  Requests\/sec:/ { print $2; exit }' "$tmp")
        p50s+=("$p50")
        p95s+=("$p95")
        p99s+=("$p99")
        rpss+=("$rps")
        # Brief pause between runs to let TIME_WAIT sockets drain.
        sleep 1
    done

    rm -f "$tmp"

    # Print median of each metric. For N runs we pick the middle value
    # after sort; for even N we take the lower middle.
    median() {
        printf '%s\n' "$@" | sort -g | awk -v n="$#" 'NR==int((n+1)/2){print; exit}'
    }
    echo
    echo "============ MEDIAN OF $runs RUNS ============"
    echo "p50:      $(median "${p50s[@]}")"
    echo "p95:      $(median "${p95s[@]}")"
    echo "p99:      $(median "${p99s[@]}")"
    echo "req/sec:  $(median "${rpss[@]}")"
    echo "all p50s: ${p50s[*]}"
    echo "all p95s: ${p95s[*]}"
    echo "all p99s: ${p99s[*]}"
    echo "all rpss: ${rpss[*]}"
}

# Timestamp for result files.
now() { date +%Y%m%dT%H%M%S; }
