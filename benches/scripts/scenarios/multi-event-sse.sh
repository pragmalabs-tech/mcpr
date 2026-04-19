#!/usr/bin/env bash
# Multi-event SSE conformance — does mcpr preserve multi-event SSE streams?
#
# Runs POST /mcp against `multi-event-mock` directly and through mcpr,
# then diffs the response bodies byte-for-byte. `extract_json_from_sse`
# returns None for multi-event streams, so today the body happens to
# pass through unchanged **only because the middleware chain silently
# bails**. Once we refactor to explicit streaming, this must still
# pass — that's the point of the test.
#
# Expected behavior:
#   - Before refactor: body byte-matches (by accident), framing metadata
#     is preserved (no reencoding happens), but `transfer-encoding:
#     chunked` becomes `content-length`.
#   - After refactor: both body match AND `transfer-encoding: chunked`
#     preserved.

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
. "$SCRIPT_DIR/../lib.sh"

require_tools curl diff
build_bins --bin multi-event-mock
trap teardown EXIT INT TERM

# Start the multi-event mock on the shared MOCK_PORT.
"$BENCH_DIR/target/release/multi-event-mock" --bind "127.0.0.1:${MOCK_PORT}" \
    >/tmp/mcpr-bench-mock.log 2>&1 &
MOCK_PID=$!
wait_http "http://127.0.0.1:${MOCK_PORT}/healthz"
start_proxy

OUT="$RESULTS_DIR/$(now)-multi-event-sse.txt"

REQ='{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"stream-progress","arguments":{}}}'

capture() {
    local url="$1" headers_file="$2" body_file="$3"
    curl -sS -X POST "$url" \
        -H 'content-type: application/json' \
        -H 'accept: application/json, text/event-stream' \
        -d "$REQ" \
        -o "$body_file" \
        -D "$headers_file" \
        --max-time 5
}

DIRECT_H=$(mktemp); DIRECT_B=$(mktemp)
PROXY_H=$(mktemp);  PROXY_B=$(mktemp)
trap 'rm -f "$DIRECT_H" "$DIRECT_B" "$PROXY_H" "$PROXY_B"; teardown' EXIT INT TERM

capture "http://127.0.0.1:${MOCK_PORT}/mcp"   "$DIRECT_H" "$DIRECT_B"
capture "http://127.0.0.1:${PROXY_PORT}/mcp" "$PROXY_H"  "$PROXY_B"

{
    echo "scenario:    multi-event-sse"
    echo "mcpr:        $($MCPR --version 2>&1 | head -1)"
    echo
    echo "=== direct upstream — response body ==="
    cat "$DIRECT_B"
    echo
    echo "=== proxied through mcpr — response body ==="
    cat "$PROXY_B"
    echo
    echo "=== direct upstream — response headers ==="
    cat "$DIRECT_H"
    echo
    echo "=== proxied through mcpr — response headers ==="
    cat "$PROXY_H"
    echo

    echo "================================================================"
    echo "checks"
    echo "================================================================"

    fail=0
    check() {
        local label="$1" cond="$2"
        if eval "$cond" >/dev/null 2>&1; then
            echo "  ok   $label"
        else
            echo "  FAIL $label"
            fail=1
        fi
    }

    check "upstream returns content-type: text/event-stream" \
        "grep -qi 'content-type: text/event-stream' '$DIRECT_H'"
    check "proxied  returns content-type: text/event-stream" \
        "grep -qi 'content-type: text/event-stream' '$PROXY_H'"

    check "proxied body byte-matches upstream body (multi-event framing preserved)" \
        "diff -q '$DIRECT_B' '$PROXY_B'"

    # Count 'data:' lines on both sides — multi-event means ≥2.
    direct_events=$(grep -c '^data:' "$DIRECT_B" || true)
    proxy_events=$(grep -c '^data:' "$PROXY_B" || true)
    check "upstream emitted multiple events (${direct_events} data: lines)" \
        "[[ $direct_events -ge 2 ]]"
    check "proxied preserved all events (${proxy_events} data: lines)" \
        "[[ $proxy_events -eq $direct_events ]]"

    # Conditional chunked-preservation: only check if upstream used chunked.
    # This mock returns a single fixed body so it serves with content-length;
    # the check is noise here. `sse-compat.sh` covers the rmcp chunked case.
    if grep -qi 'transfer-encoding: chunked' "$DIRECT_H"; then
        check "proxied preserves transfer-encoding: chunked" \
            "grep -qi 'transfer-encoding: chunked' '$PROXY_H'"
    fi

    echo
    if [[ $fail -eq 0 ]]; then
        echo "RESULT: PASS"
    else
        echo "RESULT: FAIL"
    fi
    exit $fail
} | tee "$OUT"
