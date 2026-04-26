# Proxy Command Use Cases

Task-oriented recipes for the `mcpr proxy` commands. For full flag reference see [CLI.md](../CLI.md). For config syntax see [PROXY_CONFIGURATION.md](PROXY_CONFIGURATION.md).

Every recipe assumes the daemon is running (`mcpr start`).

---

## First-time setup

Put an MCP server behind the proxy.

```bash
mcpr proxy setup              # prompts for project + auth, writes mcpr.toml
mcpr proxy run mcpr.toml      # snapshot config, daemonize, start serving
mcpr proxy status             # confirm it's up
```

`proxy run` writes a config snapshot under `~/.mcpr/proxies/<name>/`. Later commands (`start`, `restart`, `reload`) operate on that snapshot.

---

## Apply a config change

Edit `mcpr.toml`, then pick the right command for the change:

```bash
mcpr proxy reload <name> --config mcpr.toml      # zero-downtime, sessions stay alive
mcpr proxy restart <name> --config mcpr.toml     # if reload rejects (fields require restart)
```

Reload sends SIGHUP and hot-swaps. It rejects when fields like `mcp.*` change — those need a fresh process. Restart kills and respawns; sessions drop.

---

## Bring back a stopped proxy

```bash
mcpr proxy list               # see which are Stopped or Stale
mcpr proxy start <name>       # relaunch from saved snapshot
```

Use this after a crash, after `proxy stop`, or after a host reboot. No config arg — `start` always uses the snapshot. To launch with a different config, run `proxy run <new-config>` instead.

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
mcpr proxy stop --all
mcpr proxy restart --all
mcpr proxy logs                       # no name = aggregate across all proxies
```

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
