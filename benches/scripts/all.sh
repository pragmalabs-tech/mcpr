#!/usr/bin/env bash
# Runs the full scenario suite — correctness gates first, then perf.
# Correctness failures are hard errors; perf scenarios continue on
# non-zero so one noisy run doesn't hide others.

set -u
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

CORRECTNESS=(
    correctness/sse-compat.sh
    correctness/multi-event-sse.sh
    correctness/passthrough-binary.sh
)

# Heavier perf scenarios can be skipped via env for quick iterations.
PERF=(
    perf/overhead.sh
    perf/session-reuse.sh
    perf/realistic-mix.sh
)
[[ "${SKIP_STRESS:-0}" == "0" ]] && PERF+=(perf/stress.sh)
[[ "${SKIP_LATENCY:-0}" == "0" ]] && PERF+=(perf/realistic-latency.sh)

echo "################################################################"
echo "# correctness scenarios — hard gate"
echo "################################################################"
fail=0
for s in "${CORRECTNESS[@]}"; do
    echo
    echo "### $s"
    if ! "$SCRIPT_DIR/scenarios/$s"; then
        echo "!! $s FAILED"
        fail=1
    fi
done

if [[ $fail -ne 0 ]]; then
    echo
    echo "################################################################"
    echo "# correctness FAILED — skipping perf scenarios"
    echo "################################################################"
    exit 1
fi

echo
echo "################################################################"
echo "# perf scenarios — medians, directional not absolute"
echo "################################################################"
for s in "${PERF[@]}"; do
    echo
    echo "### $s"
    "$SCRIPT_DIR/scenarios/$s" || echo "!! $s exited non-zero (continuing)"
done

echo
echo "================================================================"
echo "done. Results under benches/results/ (gitignored)."
echo "================================================================"
