#!/usr/bin/env bash
# SSE compatibility — raw curl diff, direct vs proxied.
# Answers: "does mcpr byte-pass SSE (text/event-stream) responses correctly?"
#
# NOT a perf test. Verifies protocol conformance. Fails loud if mcpr
# reformats the SSE frame (which, as of v0.4.41, it does — see
# benches/README.md § Known mcpr issues).

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
. "$SCRIPT_DIR/../../lib.sh"

require_tools curl diff
build_bins --bin stateful-mock
trap teardown EXIT INT TERM

start_mock stateful
start_proxy

OUT="$RESULTS_DIR/$(now)-sse-compat.txt"

INIT='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"sse-compat","version":"0"}}}'

capture() {
    local url="$1" headers_file="$2" body_file="$3"
    curl -sS -X POST "$url" \
        -H 'content-type: application/json' \
        -H 'accept: application/json, text/event-stream' \
        -d "$INIT" \
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
    echo "scenario:    sse-compat"
    echo "mcpr:        $(mcpr --version 2>&1 | head -1)"
    echo
    echo "=== direct upstream — response headers ==="
    cat "$DIRECT_H"
    echo
    echo "=== direct upstream — response body (hexdump, first 200 bytes) ==="
    head -c 200 "$DIRECT_B" | xxd
    echo
    echo "=== direct upstream — response body (text) ==="
    cat "$DIRECT_B"
    echo
    echo "=== proxied through mcpr — response headers ==="
    cat "$PROXY_H"
    echo
    echo "=== proxied through mcpr — response body (hexdump, first 200 bytes) ==="
    head -c 200 "$PROXY_B" | xxd
    echo
    echo "=== proxied through mcpr — response body (text) ==="
    cat "$PROXY_B"
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

    # Upstream advertises SSE — proxy should too.
    check "upstream returns content-type: text/event-stream" \
        "grep -qi 'content-type: text/event-stream' '$DIRECT_H'"
    check "proxied  returns content-type: text/event-stream" \
        "grep -qi 'content-type: text/event-stream' '$PROXY_H'"

    # Streaming should be chunked, not content-length.
    check "upstream uses transfer-encoding: chunked" \
        "grep -qi 'transfer-encoding: chunked' '$DIRECT_H'"
    check "proxied  preserves transfer-encoding: chunked" \
        "grep -qi 'transfer-encoding: chunked' '$PROXY_H'"

    # Body must match byte-for-byte for byte-passing to hold.
    check "proxied body byte-matches upstream body" \
        "diff -q '$DIRECT_B' '$PROXY_B'"

    echo
    if [[ $fail -eq 0 ]]; then
        echo "RESULT: PASS  (mcpr byte-passes SSE correctly)"
    else
        echo "RESULT: FAIL  (SSE framing is being reformatted — see benches/README.md)"
    fi
    exit $fail
} | tee "$OUT"
