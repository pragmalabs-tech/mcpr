#!/bin/sh
#
# Container entrypoint for mcpr.
#
# Behaviour:
#   docker run <image>                → daemon (+ auto-launch proxy if a
#                                        config is bind-mounted at
#                                        /etc/mcpr/mcpr.toml or $MCPR_CONFIG)
#   docker run <image> <args...>      → exec `mcpr <args...>` directly
#                                        (for one-shot commands like
#                                        `version`, `validate`, `proxy run`,
#                                        or interactive shells via entrypoint
#                                        override)
#
# The daemon path exists because `mcpr proxy run` always daemonizes and
# requires the daemon to be up first. Running just `mcpr start --foreground`
# in a container would serve no traffic, so we bootstrap both.

set -eu

# Pass-through: any CLI args → run mcpr directly and exit.
if [ "$#" -gt 0 ]; then
  exec mcpr "$@"
fi

CONFIG_PATH="${MCPR_CONFIG:-/etc/mcpr/mcpr.toml}"

if [ -f "$CONFIG_PATH" ]; then
  # Background: wait for daemon to write its PID file, then launch the proxy.
  # The daemon has no HTTP endpoint, so we poll `mcpr status` for readiness.
  (
    i=0
    until mcpr status >/dev/null 2>&1; do
      i=$((i + 1))
      if [ "$i" -gt 50 ]; then
        echo "[mcpr-entrypoint] daemon did not become ready in 10s" >&2
        exit 1
      fi
      sleep 0.2
    done
    exec mcpr --config "$CONFIG_PATH" proxy run
  ) &
fi

exec mcpr start --foreground
