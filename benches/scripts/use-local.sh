#!/usr/bin/env bash
# Build the local mcpr release binary and print the path so you can use it.
# Works in any shell (bash/zsh) because it doesn't try to modify the
# calling shell's env — you do it with command substitution or eval.
#
# Usage:
#   # Pick either style, they do the same thing.
#   eval "$(scripts/use-local.sh)"                # exports MCPR_BIN in this shell
#   scripts/scenarios/fixed-overhead.sh
#
#   # Or one-shot:
#   MCPR_BIN=$(scripts/use-local.sh --print) scripts/scenarios/fixed-overhead.sh

set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
BENCH_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
REPO_DIR="$(cd "$BENCH_DIR/.." && pwd)"
MCPR_PATH="$REPO_DIR/target/release/mcpr"

PRINT_ONLY=0
[ "${1:-}" = "--print" ] && PRINT_ONLY=1

if [ "$PRINT_ONLY" -eq 0 ]; then
    echo "# building mcpr-proxy (release) from $REPO_DIR" >&2
fi
(cd "$REPO_DIR" && cargo build --release -p mcpr-proxy --bin mcpr >&2)

if [ "$PRINT_ONLY" -eq 1 ]; then
    echo "$MCPR_PATH"
else
    # Emit an export line the caller can eval. Stdout only.
    echo "export MCPR_BIN=$MCPR_PATH"
    echo "# now run any scripts/scenarios/*.sh" >&2
fi
