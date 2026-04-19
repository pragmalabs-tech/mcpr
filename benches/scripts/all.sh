#!/usr/bin/env bash
# Runs every scenario back-to-back. ~3-5 minutes on a dev laptop.

set -euo pipefail
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

SCENARIOS=(
    fixed-overhead
    realistic-overhead
    session-churn
    sse-compat
    # session-reuse — intentionally disabled until the SSE fix lands.
)

for s in "${SCENARIOS[@]}"; do
    echo
    echo "################################################################"
    echo "# $s"
    echo "################################################################"
    "$SCRIPT_DIR/scenarios/${s}.sh" || echo "!! $s exited non-zero"
done
