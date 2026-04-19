#!/usr/bin/env bash
# Passthrough binary conformance — does mcpr byte-pass non-JSON responses?
#
# Runs GET /binary (256 bytes of 0x00..0xFF, content-type:
# application/octet-stream) direct vs through mcpr. Fails if mcpr
# corrupts the body via `String::from_utf8_lossy` + `.replace()` in
# `UpstreamUrlMapMiddleware`.
#
# This path hits mcpr's `passthrough` handler (route is not /mcp). The
# current middleware gates on JSON content-type (so should be OK for
# octet-stream) but the refactor will verify this invariant holds.

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
. "$SCRIPT_DIR/../lib.sh"

require_tools curl diff cmp
build_bins --bin stateless-mock
trap teardown EXIT INT TERM

start_mock stateless
start_proxy

OUT="$RESULTS_DIR/$(now)-passthrough-binary.txt"

DIRECT_B=$(mktemp); PROXY_B=$(mktemp)
DIRECT_H=$(mktemp); PROXY_H=$(mktemp)
trap 'rm -f "$DIRECT_B" "$PROXY_B" "$DIRECT_H" "$PROXY_H"; teardown' EXIT INT TERM

curl -sS "http://127.0.0.1:${MOCK_PORT}/binary" -o "$DIRECT_B" -D "$DIRECT_H" --max-time 5
curl -sS "http://127.0.0.1:${PROXY_PORT}/binary" -o "$PROXY_B" -D "$PROXY_H" --max-time 5

{
    echo "scenario:    passthrough-binary"
    echo "mcpr:        $($MCPR --version 2>&1 | head -1)"
    echo
    echo "=== direct upstream — response headers ==="
    cat "$DIRECT_H"
    echo
    echo "=== proxied through mcpr — response headers ==="
    cat "$PROXY_H"
    echo
    echo "=== byte counts ==="
    echo "direct : $(wc -c < "$DIRECT_B") bytes"
    echo "proxied: $(wc -c < "$PROXY_B") bytes"

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

    # Precompute values — embedding `$(...)` inside a check-string that
    # contains single-quoted `$DIRECT_B` breaks expansion.
    direct_size=$(wc -c < "$DIRECT_B")
    proxy_size=$(wc -c < "$PROXY_B")

    check "upstream content-type: application/octet-stream" \
        "grep -qi 'content-type: application/octet-stream' '$DIRECT_H'"
    check "proxied content-type: application/octet-stream" \
        "grep -qi 'content-type: application/octet-stream' '$PROXY_H'"
    check "direct body is 256 bytes (got $direct_size)" "[[ $direct_size -eq 256 ]]"
    check "proxied body is 256 bytes (got $proxy_size)" "[[ $proxy_size -eq 256 ]]"
    check "proxied body byte-matches upstream body" "cmp -s '$DIRECT_B' '$PROXY_B'"

    echo
    if [[ $fail -eq 0 ]]; then
        echo "RESULT: PASS"
    else
        echo "RESULT: FAIL"
    fi
    exit $fail
} | tee "$OUT"
