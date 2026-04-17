# Docker

mcpr publishes a multi-arch Linux image to GitHub Container Registry on every release. One image serves both the daemon and a proxy — if you bind-mount a config, the proxy starts automatically.

```
ghcr.io/cptrodgers/mcpr:latest       # latest release
ghcr.io/cptrodgers/mcpr:0.4.33       # pinned semver
ghcr.io/cptrodgers/mcpr:0.4          # latest 0.4.x
```

Platforms: `linux/amd64`, `linux/arm64`.

## Quick start

```bash
cat > mcpr.toml <<'EOF'
mcp = "http://host.docker.internal:9000"
port = 3000

[runtime]
admin_bind = "127.0.0.1:9901"
EOF

docker run -d --name mcpr \
  -v "$(pwd)/mcpr.toml:/etc/mcpr/mcpr.toml:ro" \
  -v mcpr-state:/var/lib/mcpr \
  -p 3000:3000 \
  ghcr.io/cptrodgers/mcpr:latest
```

The container starts the daemon, waits for it to become ready, then launches the proxy defined in your config. Traffic flows through `http://localhost:3000`.

## How the image works

The entrypoint runs as PID 1 under `tini` for signal forwarding. Two startup paths:

| Invocation | Behavior |
|---|---|
| `docker run <image>` with a config at `/etc/mcpr/mcpr.toml` | Starts the daemon, waits for readiness, launches `mcpr proxy run` against that config. |
| `docker run <image>` with no config | Starts the daemon only. Attach a proxy later via `docker exec <container> mcpr --config /path proxy run`. |
| `docker run <image> <subcommand>` (e.g. `version`, `validate -c /etc/mcpr/mcpr.toml`) | Runs `mcpr <subcommand>` directly and exits. |

`mcpr proxy run` always daemonizes and requires the daemon to be up first. The entrypoint handles this so a single `docker run` command serves traffic.

## Ports

| Port | Purpose | Default bind |
|---|---|---|
| `3000` | Proxy listener — set by `port` in `mcpr.toml` | `0.0.0.0` (override in config) |
| `9901` | Proxy admin API — `/healthz`, `/ready`, `/version` | `127.0.0.1` |

The admin port defaults to localhost. To expose it to Docker health probes from outside the container or to Kubernetes liveness/readiness probes, set `admin_bind = "0.0.0.0:9901"` in `[runtime]`.

## Volumes

| Path | Purpose |
|---|---|
| `/etc/mcpr/mcpr.toml` | Proxy config — bind-mount read-only |
| `/var/lib/mcpr` | State directory — SQLite store, PID files, lockfiles, config snapshots, logs |

State resolves via the `HOME` environment variable, which the image sets to `/var/lib/mcpr`. mcpr writes everything under `/var/lib/mcpr/.mcpr/`. Declare a named volume or bind-mount to persist state across container restarts.

## Environment variables

| Variable | Default | Purpose |
|---|---|---|
| `MCPR_CONFIG` | `/etc/mcpr/mcpr.toml` | Path the entrypoint checks to decide whether to auto-launch a proxy |
| `HOME` | `/var/lib/mcpr` | Parent directory for `.mcpr/` state |
| `MCPR_DB` | — | Overrides the SQLite file path (normally `$HOME/.mcpr/store.db`) |
| `MCPR_NO_TUI` | `1` | Disables the interactive TUI. Leave set in non-tty environments. |

## Health probes

The proxy admin server exposes two endpoints:

- `GET /healthz` — returns `200 {"status":"ok"}`. Returns `503` during shutdown drain.
- `GET /ready` — returns `200 {"status":"ready"}` when the upstream MCP server is reachable. Returns `503` while draining or when the upstream is disconnected.

The image includes a `HEALTHCHECK` that curls `/healthz` every 30 seconds. Container health appears in `docker ps`:

```bash
$ docker ps --format '{{.Names}} {{.Status}}'
mcpr  Up 45 seconds (healthy)
```

For Kubernetes, use separate probes:

```yaml
livenessProbe:
  httpGet: { path: /healthz, port: 9901 }
  initialDelaySeconds: 15
readinessProbe:
  httpGet: { path: /ready, port: 9901 }
  initialDelaySeconds: 5
```

Kubernetes probes run against the pod IP, not `127.0.0.1`, so set `admin_bind = "0.0.0.0:9901"` in the config.

## Signals

SIGTERM triggers a graceful drain. `tini` forwards the signal to the daemon, which stops all running proxies (draining in-flight requests up to `drain_timeout`, default 30 seconds) before exiting. `docker stop` and Kubernetes pod termination both work as expected.

## Non-root

The image runs as `mcpr` (UID/GID `10001`). This works with Kubernetes `runAsNonRoot: true`. The state volume must be writable by UID `10001`:

```yaml
securityContext:
  runAsUser: 10001
  runAsGroup: 10001
  fsGroup: 10001
```

## One-shot commands

Any argument passed to `docker run` is forwarded to `mcpr`:

```bash
docker run --rm ghcr.io/cptrodgers/mcpr:latest version
# → {"target":"unknown","version":"0.4.33"}

docker run --rm -v "$(pwd)/mcpr.toml:/etc/mcpr/mcpr.toml:ro" \
  ghcr.io/cptrodgers/mcpr:latest validate -c /etc/mcpr/mcpr.toml

docker run --rm -v mcpr-state:/var/lib/mcpr \
  ghcr.io/cptrodgers/mcpr:latest store stats
```

`docker exec` into a running container works the same way:

```bash
docker exec mcpr mcpr status
docker exec mcpr mcpr proxy logs --tail 20
docker exec mcpr mcpr proxy status
```

## docker-compose

```yaml
services:
  mcpr:
    image: ghcr.io/cptrodgers/mcpr:latest
    restart: unless-stopped
    ports:
      - "3000:3000"
    volumes:
      - ./mcpr.toml:/etc/mcpr/mcpr.toml:ro
      - mcpr-state:/var/lib/mcpr

volumes:
  mcpr-state:
```

## Connecting to an MCP server on the host

From inside a container, the host is reachable at `host.docker.internal` on Docker Desktop (macOS, Windows). On Linux, add `--add-host=host.docker.internal:host-gateway` to `docker run`, or use the host's LAN address.

```toml
# mcpr.toml — upstream is an MCP server on the host
mcp = "http://host.docker.internal:9000"
```

## Image tags

Release CI pushes four tags per release:

- `latest` — the most recent release
- `X.Y.Z` — exact version (e.g. `0.4.33`)
- `X.Y` — latest patch of a minor line (e.g. `0.4`)
- `sha-<short>` — the commit hash (for reproducible pulls)

Pin to `X.Y.Z` in production. `latest` is for evaluation.

## Gotchas

- **`mcpr proxy run` uses `--config`, not a positional argument.** Both inside and outside the container, the form is `mcpr --config /path/mcpr.toml proxy run`. The entrypoint handles this when auto-launching; manual invocations must match.
- **The default `admin_bind` is `127.0.0.1:9901`.** The built-in `HEALTHCHECK` works (it runs inside the container), but external probes need `admin_bind = "0.0.0.0:9901"`.
- **State lives at `/var/lib/mcpr/.mcpr/`, not `/var/lib/mcpr/`.** The `.mcpr` subdirectory is appended by mcpr. The volume mount point is the parent, not the state directory itself.
- **No `procps` in the image.** `docker exec ... ps` fails. Use `mcpr status` and `mcpr proxy list` for process information.
