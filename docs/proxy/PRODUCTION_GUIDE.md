# Production Configuration Guide

This document is the operator's guide to running mcpr in production. It covers **sizing, tuning, observability, operations, and the known limits** — everything that isn't in [CONFIGURATION.md](PROXY_CONFIGURATION.md) (the `mcpr.toml` syntax reference) or [CLI.md](../CLI.md) (command reference).

If this is your first mcpr deployment, read in order:

1. Sizing — pick the right instance
2. Configuration — the settings that actually move the needle
3. OS tuning — Linux sysctls for high-concurrency
4. Observability — sinks, logs, alerts
5. Operations — deploy, rotate, prune
6. Security — network boundaries + auth
7. Known limits — what breaks and at what scale

---

## 1. Sizing

### Capacity measured on M4 macOS loopback (v0.4.42)

| Concurrent clients | Proxy throughput | p50 latency | p99 latency |
|-------------------:|-----------------:|------------:|------------:|
| 1                  | 16 k rps         | 58 µs       | 85 µs       |
| 10                 | 50 k rps         | 180 µs      | 330 µs      |
| 30                 | 75 k rps         | 360 µs      | 970 µs      |
| 50                 | 76 k rps         | 600 µs      | 1.6 ms      |

Linux with widened port range should scale higher; exact ceiling is hardware-bound, not proxy-bound.

### Real-world estimation

**Realistic MCP clients do 1–10 rps per session**, not the 1000+ rps the bench pushes. To size for a real deployment:

```
required_throughput ≈ concurrent_sessions × per_session_rps
```

Examples:

| Deployment shape                   | Aggregate rps | mcpr sizing                  |
|------------------------------------|--------------:|------------------------------|
| 50 desktop clients, 5 rps each     | 250 rps       | Any shared host. 1 vCPU fine. |
| 500 agent clients, 2 rps each      | 1,000 rps     | 1-2 vCPU, 512 MB RAM.        |
| 5,000 clients, 2 rps each          | 10,000 rps    | 4 vCPU, 2 GB RAM. Plus Linux tuning. |
| 50,000 clients (multi-tenant SaaS) | 100,000 rps   | Horizontal scale — run N instances behind a load balancer. One mcpr box won't do this. |

### Resource breakdown per instance

Typical steady-state on a 10k rps deployment:

- **CPU**: 2-4 cores active out of available. Tokio schedules across all cores automatically; no config needed. Pin to specific cores only if you have a noisy neighbor problem.
- **Memory**:
  - `MemorySessionStore`: ~8 KB per active session (DashMap entry + session metadata)
  - Event bus channel: 10k events × ~500 B = ~5 MB
  - Response buffers: capped by `max_response_body_size` × in-flight requests
  - Estimate: **`100 MB + 10 KB × active_sessions + max_response_body × 100`**
- **File descriptors**: one per active inbound + one per in-flight upstream + pool idle. Default `ulimit -n 1024` is usually enough; raise to 10k+ for multi-tenant.
- **Disk**: sqlite store grows with event volume. See § Operations for pruning.

---

## 2. Configuration — the settings that matter

Start with the **minimal** config (one line: `mcp = "..."`). The defaults are production-safe. Tune from there only when the numbers below say you should.

### Concurrency and pool

```toml
# In `mcpr.toml`:
max_concurrent_upstream = 100      # default — semaphore hard cap
```

| When to change | To |
|----------------|----|
| Upstream takes > 500 ms per request AND you have > 100 concurrent active sessions | Raise to `300-500`. The semaphore limits in-flight upstream requests; a slow upstream compounds with high session count to hit the 100 cap. |
| Low-RPS deployment (< 100 aggregate rps) | Leave at 100. |

Note: the **idle pool** (`pool_max_idle_per_host = 64`) is currently hardcoded in the source. Already sized to the p99 < 1 ms sweet spot; don't worry about it unless profiling says to.

### Body size limits

```toml
max_request_body_size = 5_242_880      # 5 MB  — default
max_response_body_size = 10_485_760    # 10 MB — default
```

These are **safety caps**. Requests/responses exceeding them return 413. Raise only if your tools return > 10 MB payloads (unusual — most tool responses are < 10 KB).

**Memory budget warning**: each in-flight buffered request allocates up to `max_response_body_size`. At 100 concurrent requests and 100 MB caps, you'd see 10 GB peak memory. Size caps against your RAM budget.

### Timeouts

```toml
connect_timeout = 5         # seconds — default
request_timeout = 30        # seconds — default
```

| Upstream type                    | Recommended `request_timeout` |
|----------------------------------|-------------------------------|
| Local MCP server (stdio-wrapped) | 30 s (default) |
| HTTP-backed tool (< 1 s typical) | 30 s |
| LLM sampling / code exec         | 60–180 s |
| Long-running task tools          | 300 s + (also set client-side timeouts to match) |

SSE streaming requests ignore `request_timeout` by design — streams need to stay open indefinitely for server push.

### Logging

```toml
[runtime]
log_format = "json"    # default — machine-parseable
# log_format = "pretty"  # human-readable, use for local dev
```

**Always `"json"` in production.** Pipe to your log aggregator (Loki, ELK, Datadog, CloudWatch). Pretty format is for interactive terminals.

### CSP and widgets

See [CSP.md](CSP.md). If you don't use widgets, `[csp]` is fine at defaults. Misconfiguring CSP breaks widget rendering silently — lean on defaults unless you know what you need.

### Storage

```toml
[store]
path = "/var/lib/mcpr/store.db"     # default: ~/.mcpr/store.db
```

**Put the store on a dedicated, fast disk** if you expect high event volume. Events write in batches every 5 s; sqlite tolerates this well but the DB grows unbounded — see § Operations for pruning.

Environment variable override: `MCPR_DB=/path/to/store.db`.

---

## 3. OS tuning (Linux)

For any deployment handling > 1000 concurrent clients or > 5000 aggregate rps, tune the host. Apply via `sysctl`, `systemd.service`, or container-level.

### Port range + TIME_WAIT reuse

```bash
# /etc/sysctl.d/mcpr.conf
net.ipv4.ip_local_port_range = 10240 65535
net.ipv4.tcp_tw_reuse = 1
net.ipv4.tcp_fin_timeout = 30
```

**Why**: high-rps loopback or localhost-upstream forwarding fills the ephemeral port range quickly when connections recycle. `tcp_tw_reuse` lets new connections bind ports still in TIME_WAIT if they're safe to reuse.

### Socket buffer + accept queue

```bash
net.core.somaxconn = 4096
net.ipv4.tcp_max_syn_backlog = 4096
```

**Why**: SYN bursts during a client-fleet reconnect storm otherwise drop at the kernel level before mcpr sees them.

### File descriptor limits

```bash
# /etc/security/limits.d/mcpr.conf
mcpr soft nofile 65536
mcpr hard nofile 65536
```

Or in systemd:

```ini
[Service]
LimitNOFILE=65536
```

**Why**: each active session + in-flight upstream + idle pool connection is a fd. Default `ulimit 1024` bites at ~500 concurrent sessions.

### CPU governor

For dedicated mcpr hosts:

```bash
cpupower frequency-set --governor performance
```

**Why**: `ondemand` / `powersave` governors add 100-500 µs of jitter at request start. Not worth it on shared hosts; matters on dedicated ones.

---

## 4. Upstream server configuration

mcpr is only as good as your upstream. Tune both sides:

### Keep-alive alignment

```
mcpr's pool_idle_timeout = 90 s (default)
upstream's keep-alive timeout MUST be ≥ 90 s
```

If upstream kills idle connections at 60 s while mcpr expects them alive at 90 s, mcpr will occasionally pick up a dead connection and retry — showing up as p99 spikes in your latency graphs.

Example upstream-side settings to match:

- **axum / hyper**: `Server::builder().tcp_keepalive(Some(Duration::from_secs(120)))`
- **nginx as upstream fronting**: `keepalive_timeout 120s;`
- **Node.js http.Server**: `server.keepAliveTimeout = 120_000`

### HTTP/2 support

mcpr's reqwest client negotiates HTTP/2 if upstream advertises it. **Enable HTTP/2 on your upstream** — one TCP connection multiplexes unlimited concurrent requests, and sidesteps ephemeral port pressure.

Biggest single operational win for high-concurrency deployments. rmcp with axum + h2 feature is the easy path.

### Upstream timeouts

Set upstream request timeouts slightly below mcpr's `request_timeout`. Gives upstream a chance to return a 504 or error response before mcpr gives up — cleaner error events than "upstream error" transport failures.

---

## 5. Observability

### Event sinks

mcpr emits proxy events (request, session-start, session-end, heartbeat, schema-change) to configurable sinks.

| Sink              | When to enable in production |
|-------------------|------------------------------|
| **stderr** (json) | Always on. Pipe to log aggregator. |
| **sqlite**        | Local diagnostics. Turn off on multi-instance deployments (per-instance DB isn't useful). Grows unbounded — see below. |
| **cloud**         | mcpr-cloud dashboard. Use if you subscribe. |

Configure in `mcpr.toml`:

```toml
[runtime]
log_format = "json"

[store]
# Disable by setting path = "/dev/null" or remove the section.
# Better: keep it, but run `mcpr store vacuum` on cron.
```

### What to monitor + alert on

Run `mcpr proxy status --json` periodically and alert on:

| Metric            | Warning                                             | Critical     |
|-------------------|-----------------------------------------------------|--------------|
| Error rate        | > 1 %                                               | > 5 %        |
| Total requests/s  | > 80 % of `max_concurrent_upstream × upstream_rps`  | > 95 %       |
| Sessions (active) | unbounded growth over 24 h                          | > RAM budget |
| Average latency   | 2× normal                                           | 5× normal    |
| p95 latency       | 2× normal                                           | 5× normal    |

For detailed per-stage timing investigation (**off by default** — zero cost when disabled, ~1 % overhead when enabled):

```bash
MCPR_STAGE_TIMING=1 mcpr proxy run /etc/mcpr.toml
# Then analyze:
tail -f ~/.mcpr/proxies/<name>/proxy.log | jq -c 'select(.stage_timings)'
```

See [timing module source](../../crates/mcpr-core/src/timing.rs) for the envvar contract and available stages.

### Log aggregation

The stderr JSON format emits one event per line. Every major aggregator (Loki, ELK, Datadog, CloudWatch) can parse it. Key fields to index:

- `proxy` — instance name, required for multi-instance deployments
- `session_id` — correlate requests across sessions
- `mcp_method` — method-level breakdowns
- `status`, `latency_us`, `upstream_us` — SLI metrics
- `note` — transform tags (`rewritten`, `sse`, `streamed`, `passthrough`, `upstream error`)

---

## 6. Operations

### Deployment

mcpr ships as a single binary and runs as a sidecar to your MCP server. The launched PID is the proxy itself, so a process supervisor (systemd, launchd, Docker, your Node app) owns its lifecycle.

**1. Foreground (recommended — Docker, Kubernetes, systemd, embedded)**

```bash
mcpr proxy run /etc/mcpr.toml
```

Systemd unit skeleton:

```ini
[Unit]
Description=mcpr proxy
After=network.target

[Service]
Type=simple
User=mcpr
ExecStart=/usr/local/bin/mcpr proxy run /etc/mcpr.toml
Restart=on-failure
RestartSec=5
LimitNOFILE=65536
TimeoutStopSec=35

[Install]
WantedBy=multi-user.target
```

**2. From a terminal (foreground, dev / debug)**

```bash
mcpr proxy run /etc/mcpr.toml          # blocks the terminal; Ctrl-C to stop
mcpr proxy list                        # from another terminal — see what's running
mcpr proxy stop <name>                 # SIGTERM + drain
```

The proxy writes a lockfile at `~/.mcpr/proxies/<name>/lock` so `list / stop / reload` work from another terminal even though the original launch is foreground. To run multiple proxies on one box, launch each from its own terminal (or use a process supervisor like `tmux` / `pm2` / systemd).

### Graceful shutdown

mcpr handles `SIGTERM` / `SIGINT` cleanly:

1. Stops accepting new requests
2. Drains in-flight requests (up to 30 s)
3. Flushes event bus
4. Closes sqlite store

Give your orchestrator at least 30 s `stopGracePeriod` / `TimeoutStopSec`.

### Log rotation

mcpr writes structured JSON to stderr; the host supervisor (systemd journal, Docker JSON driver, your Node process) is responsible for capture and rotation. Configure rotation there.

### Sqlite store pruning

```bash
mcpr store stats             # show size + row counts
mcpr store vacuum --before 7d  # delete events older than 7 days, reclaim disk
```

Cron schedule for a busy proxy:

```cron
0 3 * * * mcpr store vacuum --before 30d
```

Without pruning, the store grows indefinitely — at 1000 rps + 500 bytes/event, that's ~43 GB/day.

### Config changes

Two paths, depending on whether sessions can tolerate a restart:

**Hot reload** — `[csp]` and widget-scoped CSP overrides only, no session drop:

```bash
mcpr proxy reload <name> -c /etc/mcpr.toml
```

The proxy re-reads the snapshot on SIGHUP and atomically swaps CSP. Changes to any other field (`mcp`, `port`, `widgets`, `tunnel.*`, timeouts, body limits, `[cloud]`, `[runtime]`) are rejected — the proxy logs which field failed the safety check and keeps running on the old config.

**Full restart** — any other config change, drops in-flight sessions:

```bash
mcpr proxy stop <name>                  # SIGTERM, drains in-flight requests
# Your supervisor (systemd Restart=on-failure / Docker / k8s / shell loop)
# respawns mcpr proxy run with the new config.
```

mcpr does not respawn proxies itself — that's the host supervisor's job. `reload` requires `-c` and always applies an explicit file.

---

## 7. Security

### Admin API binding

```toml
[runtime]
admin_bind = "127.0.0.1:9901"      # default — localhost only
# admin_bind = "none"              # disable entirely
```

**Never bind `admin_bind` to `0.0.0.0` or a public interface.** The admin API exposes `/healthz`, `/ready`, and internal stats. If you need external health checks, front mcpr with a reverse proxy that exposes only the paths you want.

### CORS

`tower-http::CorsLayer` is active by default with permissive settings (`Allow-Origin: *`). For production behind a reverse proxy that sets its own CORS, or when mcpr must restrict origins:

- Edit `mcpr-cli/src/main.rs` `build_app()` to configure the allowed origins list (currently requires a rebuild — config exposure is on the roadmap).

### TLS

**mcpr terminates HTTP, not HTTPS.** For public deployments, front it with a reverse proxy (nginx, Caddy, Cloud LB) that terminates TLS. Pattern:

```
Internet → Caddy (TLS, rate limit) → mcpr → upstream
```

Don't expose port 3000 directly.

### Upstream auth

mcpr forwards the `Authorization` header transparently — whatever your client sends passes through to upstream. For auth at the proxy layer (e.g., API key validation before forwarding), you'd add a request middleware. Not yet built-in.

---

## 8. Known limits + workarounds

### "I'm seeing p99 spikes during peak load"

First suspect: **TIME_WAIT exhaustion** on the upstream connection side. Check:

```bash
ss -tan | awk '{print $1}' | sort | uniq -c
```

If `TIME-WAIT` count is > 20,000, apply `net.ipv4.tcp_tw_reuse = 1` from § 3. Re-measure.

Second suspect: **upstream slowness**. Check `upstream_us` vs `latency_us` in events. If upstream_us is > 90 % of latency_us, it's not mcpr.

Third: **event bus backpressure**. At > 10,000 events/sec the bounded channel (10k capacity) fills and events drop. Reduce logging verbosity or scale horizontally.

### "Memory grows over time"

Usually **session leak** — clients not sending `DELETE /mcp` on disconnect. Check `mcpr proxy status` → `active sessions`. If it grows unbounded, two options:

1. Client-side: add explicit DELETE on disconnect.
2. Proxy-side: shorter session TTL (currently not configurable — on the roadmap).

### "Throughput stopped scaling at N concurrent"

- N ≈ 50 on macOS loopback → OS ephemeral port limit. Production Linux should not hit this.
- N ≈ 100 → the `max_concurrent_upstream` semaphore. Raise to 300-500 if your upstream can handle it.
- N ≈ 1000+ on Linux → you're hitting something we haven't measured. File an issue with `mcpr proxy status` output and a `samply` profile.

### "Proxy eats one full core but requests are stuck"

Likely **blocking I/O on the async runtime**. Check:

- Upstream returning giant responses (> `max_response_body_size`)?
- Cloud sink can't reach the cloud endpoint and backs up?
- sqlite store write latency spiking?

Run with `MCPR_STAGE_TIMING=1` and look for a specific stage dominating.

### "After a deploy, some clients get 'session not found'"

mcpr's session store is per-proxy-instance and in-memory. Restart invalidates all sessions. Clients must re-initialize.

For multi-instance deployments:

- **Session stickiness** at the LB layer (source IP hash or session ID header routing) keeps clients pinned to one instance.
- **Or**: persistent session backend (not yet implemented — roadmap item).

---

## Quick reference — what to set in production

```toml
# mcpr.toml  (minimal production)
mcp = "https://your-upstream.internal/mcp"
max_concurrent_upstream = 100         # default, raise if upstream is slow
request_timeout = 60                  # tune to upstream tail latency

[runtime]
log_format = "json"
admin_bind = "127.0.0.1:9901"         # never public

[store]
path = "/var/lib/mcpr/store.db"
```

```bash
# OS (Linux)
sysctl net.ipv4.ip_local_port_range="10240 65535"
sysctl net.ipv4.tcp_tw_reuse=1
ulimit -n 65536

# Env vars
MCPR_DB=/var/lib/mcpr/store.db        # store path override (same as config)
# MCPR_STAGE_TIMING=1                 # NEVER enable in production sustained
```

```bash
# Cron
0 3 * * * mcpr store vacuum --before 30d
```

That's 90% of what you need. Tune further only if numbers say to.
