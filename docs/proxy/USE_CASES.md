# Proxy Command Use Cases

Task-oriented recipes for the `mcpr proxy` commands. For full flag reference see [CLI.md](../CLI.md). For config syntax see [PROXY_CONFIGURATION.md](PROXY_CONFIGURATION.md).

`mcpr proxy run` always blocks the current terminal. Use a process supervisor (systemd, Docker, your Node app, `tmux`) to keep it alive — mcpr is the sidecar, not the supervisor.

---

## First-time setup

Put an MCP server behind the proxy.

```bash
mcpr proxy setup                          # prompts for project + auth, writes mcpr.toml
mcpr proxy run mcpr.toml                  # foreground; Ctrl-C to stop
# from another terminal:
mcpr proxy status                         # confirm it's up
```

`proxy run` writes a config snapshot at `~/.mcpr/proxies/<name>/config.toml` and a lockfile so `proxy list / stop / reload` can find it from another terminal.

---

## Apply a config change

Edit `mcpr.toml`, then:

```bash
mcpr proxy reload <name> --config mcpr.toml      # zero-downtime, sessions stay alive
```

Reload sends SIGHUP and hot-swaps `[csp]` settings. It rejects when fields like `mcp.*` change — those need a fresh process. For those, stop the proxy and let your supervisor respawn it:

```bash
mcpr proxy stop <name>                            # SIGTERM, drains
# systemd / Docker / your shell loop respawns mcpr proxy run with the new config
```

---

## Debug a failing or slow tool call

```bash
mcpr proxy logs <name> --status error --since 1h
mcpr proxy logs <name> --error-code -32601           # JSON-RPC method not found
mcpr proxy slow <name> --threshold 1s --tool search
mcpr proxy session <session_id>                      # full request timeline for one session
```

Session IDs accept prefixes (first 8 chars work).

---

## Watch traffic live

```bash
mcpr proxy logs <name> --follow
mcpr proxy slow <name> --follow --threshold 500ms
mcpr proxy status --since 5m
```

`--follow` polls every 500ms (logs) or 1s (slow). Pair with `--json` for piping into `jq`.

---

## Audit tools and clients

Find tools the upstream lists but no client calls:

```bash
mcpr proxy schema <name> --unused --since 30d
```

Track upstream schema drift over time:

```bash
mcpr proxy schema <name> --changes
```

See which AI clients are connecting:

```bash
mcpr proxy clients <name> --since 7d
```

---

## Manage multiple proxies

```bash
mcpr proxy list                       # status of all known proxies
mcpr proxy stop --all                 # SIGTERM every running proxy
mcpr proxy logs                       # no name = aggregate across all proxies
```

To restart all of them, stop and let your supervisor respawn each. mcpr does not own restart.

---

## Decommission a proxy

```bash
mcpr proxy stop <name>
mcpr proxy delete <name>              # removes snapshot + lock + on-disk state
```

`delete` requires the proxy to be stopped. Use `-y` to skip confirmation.

---

## Script with JSON output

Every observability command supports `--json`. Streams (`logs`, `slow`) emit NDJSON.

```bash
mcpr proxy logs <name> --status error --since 1h --json | jq '.tool'
mcpr proxy list --json | jq '.[] | select(.status == "stale")'
```
