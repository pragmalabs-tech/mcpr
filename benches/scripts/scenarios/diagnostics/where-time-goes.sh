#!/usr/bin/env bash
# Per-stage timing breakdown for the buffered handler.
#
# Runs a `tools/call` load against the stateless mock through the
# buffered-capable path (we force buffering by using the `tools/list`
# payload, which always classifies as McpPostBuffer). Each request's
# stage timings land in the proxy's JSON event log; this script
# aggregates medians + p95 per stage and prints a table so you can see
# *which stage* is the biggest contributor.
#
# Stages reported come from `mcpr_core::event::StageTimings`. Sum of
# median per-stage µs ≈ median latency_us − median upstream_us, with a
# small residual for unaccounted work (axum accept + build_response).

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
. "$SCRIPT_DIR/../../lib.sh"

DURATION="${DURATION:-8s}"
CONNECTIONS="${CONNECTIONS:-20}"
PAYLOAD="${PAYLOAD:-tools_list}"  # must classify as McpPostBuffer

require_tools oha jq
build_bins --bin stateless-mock
trap teardown EXIT INT TERM

# Per-stage timings are gated on `MCPR_STAGE_TIMING` (see
# `mcpr-core::timing`). Export before starting the proxy so the daemon
# propagates it to the spawned proxy process.
export MCPR_STAGE_TIMING=1

start_mock stateless
start_proxy

PAYLOAD_FILE="$BENCH_DIR/payloads/${PAYLOAD}.json"
[[ -f "$PAYLOAD_FILE" ]] || { echo "missing $PAYLOAD_FILE" >&2; exit 2; }
BODY=$(cat "$PAYLOAD_FILE")

# The proxy's JSON event log. We tail it after each run to pick up only
# this run's events.
PROXY_LOG="$HOME/.mcpr/proxies/${PROXY_NAME}/proxy.log"
[[ -f "$PROXY_LOG" ]] || { echo "missing $PROXY_LOG — did mcpr proxy start?" >&2; exit 2; }
LOG_START=$(wc -l < "$PROXY_LOG")

OUT="$RESULTS_DIR/$(now)-where-time-goes.txt"

{
    echo "scenario:    where-time-goes"
    echo "payload:     $PAYLOAD"
    echo "duration:    $DURATION"
    echo "connections: $CONNECTIONS"
    echo "mcpr:        $($MCPR --version 2>&1 | head -1)"
    echo

    echo "==> warmup (2s)"
    oha_run "http://127.0.0.1:${PROXY_PORT}/mcp" "2s" "$CONNECTIONS" "$BODY" >/dev/null

    echo "==> measuring ($DURATION)"
    oha_run "http://127.0.0.1:${PROXY_PORT}/mcp" "$DURATION" "$CONNECTIONS" "$BODY" \
        | grep -E 'Success rate|Requests/sec|^  50\.00|^  95\.00|^  99\.00'
    echo

    echo "==> parsing per-stage timings from $PROXY_LOG"

    # Collect only this run's events (lines added since LOG_START). Filter
    # to request events with stage_timings populated.
    tail -n +"$((LOG_START + 1))" "$PROXY_LOG" \
        | jq -c 'select(.type == "request") | select(.stage_timings)' \
        > /tmp/mcpr-bench-events.jsonl

    count=$(wc -l < /tmp/mcpr-bench-events.jsonl | tr -d ' ')
    echo "samples with timings: $count"
    if [[ "$count" -lt 10 ]]; then
        echo "!! not enough samples, aborting aggregation"
        exit 1
    fi

    echo
    printf '%-22s %10s %10s %10s %10s\n' stage count p50_us p95_us max_us
    printf '%s\n' '----------------------------------------------------------------------'

    # The wire shape is a list of { name, elapsed_us } entries.
    # `jq` groups by name across all events, then reports count / p50 /
    # p95 / max for each stage name that actually appeared.
    jq -s -r '
        [.[] | .stage_timings[]?]
        | group_by(.name)
        | map({
            name: .[0].name,
            count: length,
            values: (map(.elapsed_us) | sort)
          })
        | map({
            name,
            count,
            p50: .values[(.count / 2 | floor)],
            p95: .values[(.count * 0.95 | floor)],
            max: (.values | max)
          })
        | .[]
        | "\(.name)\t\(.count)\t\(.p50)\t\(.p95)\t\(.max)"
    ' /tmp/mcpr-bench-events.jsonl \
    | while IFS=$'\t' read -r name count p50 p95 mx; do
        printf '%-22s %10s %10s %10s %10s\n' "$name" "$count" "$p50" "$p95" "$mx"
    done

    echo
    echo "==> overall request latency (from RequestEvent.latency_us)"
    jq -r '.latency_us' /tmp/mcpr-bench-events.jsonl \
        | jq -s '
            sort as $s
            | {
                count: length,
                p50_us: $s[length/2|floor],
                p95_us: $s[length * 0.95 | floor],
                max_us: max
              }
        '

    echo
    echo "==> upstream contribution (RequestEvent.upstream_us)"
    jq -r '.upstream_us // empty' /tmp/mcpr-bench-events.jsonl \
        | jq -s '
            sort as $s
            | {
                count: length,
                p50_us: $s[length/2|floor],
                p95_us: $s[length * 0.95 | floor]
              }
        '

    rm -f /tmp/mcpr-bench-events.jsonl
} | tee "$OUT"

echo
echo "result saved: $OUT"
