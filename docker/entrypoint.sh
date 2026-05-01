#!/bin/sh
#
# Container entrypoint for mcpr.
#
# Behaviour:
#   docker run <image>                → exec `mcpr proxy run <config>`
#                                        (foreground; the container's PID 1
#                                        is the proxy itself, so SIGTERM /
#                                        Docker stop drains gracefully)
#   docker run <image> <args...>      → exec `mcpr <args...>` directly
#                                        (one-shot commands like `version`,
#                                        `validate`, interactive shells via
#                                        entrypoint override)
#
# Config path: $MCPR_CONFIG, falling back to /etc/mcpr/mcpr.toml.

set -eu

# Pass-through: any CLI args → run mcpr directly and exit.
if [ "$#" -gt 0 ]; then
  exec mcpr "$@"
fi

CONFIG_PATH="${MCPR_CONFIG:-/etc/mcpr/mcpr.toml}"

if [ ! -f "$CONFIG_PATH" ]; then
  echo "[mcpr-entrypoint] no config at $CONFIG_PATH; set MCPR_CONFIG or bind-mount one" >&2
  exit 1
fi

exec mcpr proxy run "$CONFIG_PATH"
