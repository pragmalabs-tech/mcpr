# Docker

mcpr publishes a multi-arch Linux image to GitHub Container Registry on every release. The container runs `mcpr proxy run <config>` as PID 1 — bind-mount your config and it serves traffic.

```
ghcr.io/pragmalabs-tech/mcpr:latest       # latest release
ghcr.io/pragmalabs-tech/mcpr:0.5.0        # pinned semver
ghcr.io/pragmalabs-tech/mcpr:0.5          # latest 0.5.x
```

Platforms: `linux/amd64`, `linux/arm64`.

## Quick start

```bash
cat > mcpr.toml <<'EOF'
mcp = "http://host.docker.internal:9000"
port = 3000
EOF

docker run -d --name mcpr \
  -v "$(pwd)/mcpr.toml:/etc/mcpr/mcpr.toml:ro" \
  -v mcpr-state:/var/lib/mcpr \
  -p 3000:3000 \
  ghcr.io/pragmalabs-tech/mcpr:latest
```

The container execs `mcpr proxy run` against your config — that process becomes PID 1. Traffic flows through `http://localhost:3000`.

## How the image works

The entrypoint runs as PID 1 under `tini` for signal forwarding, then `exec`s the proxy. Two startup paths:

| Invocation | Behavior |
|---|---|
| `docker run <image>` with a config at `/etc/mcpr/mcpr.toml` (or `MCPR_CONFIG`) | Execs `mcpr proxy run <config>` in the foreground. Container PID 1 IS the proxy. |
| `docker run <image>` with no config | Exits 1 with a hint — set `MCPR_CONFIG` or bind-mount a config. |
| `docker run <image> <subcommand>` (e.g. `version`, `validate -c /etc/mcpr/mcpr.toml`) | Runs `mcpr <subcommand>` directly and exits. |

Because `mcpr proxy run` is foreground by default, `docker stop` translates to a SIGTERM straight to the proxy, which drains in-flight requests up to `runtime.drain_timeout` (default 30s) before exiting. There is no daemon any more — the container orchestrator owns the lifecycle.

## Ports

| Port | Purpose | Default bind |
|---|---|---|
| `3000` | Proxy listener — set by `port` in `mcpr.toml` | `0.0.0.0` (override in config) |

## Volumes

| Path | Purpose |
|---|---|
| `/etc/mcpr/mcpr.toml` | Proxy config — bind-mount read-only |
| `/var/lib/mcpr` | State directory — SQLite store, lockfiles, config snapshots, logs |

State resolves via the `HOME` environment variable, which the image sets to `/var/lib/mcpr`. mcpr writes everything under `/var/lib/mcpr/.mcpr/`. Declare a named volume or bind-mount to persist state across container restarts.

## Environment variables

| Variable | Default | Purpose |
|---|---|---|
| `MCPR_CONFIG` | `/etc/mcpr/mcpr.toml` | Path the entrypoint checks to decide whether to auto-launch a proxy |
| `HOME` | `/var/lib/mcpr` | Parent directory for `.mcpr/` state |
| `MCPR_DB` | — | Overrides the SQLite file path (normally `$HOME/.mcpr/store.db`) |
| `MCPR_NO_TUI` | `1` | Disables the interactive TUI. Leave set in non-tty environments. |

## Signals

SIGTERM triggers a graceful drain. `tini` forwards the signal to the proxy (PID 1), which stops accepting new requests and finishes in-flight ones up to `runtime.drain_timeout` (default 30 seconds) before exiting. `docker stop` and Kubernetes pod termination both work as expected.

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
docker run --rm ghcr.io/pragmalabs-tech/mcpr:latest version
# → {"target":"unknown","version":"0.5.0"}

docker run --rm -v "$(pwd)/mcpr.toml:/etc/mcpr/mcpr.toml:ro" \
  ghcr.io/pragmalabs-tech/mcpr:latest validate -c /etc/mcpr/mcpr.toml

docker run --rm -v mcpr-state:/var/lib/mcpr \
  ghcr.io/pragmalabs-tech/mcpr:latest store stats
```

`docker exec` into a running container works the same way:

```bash
docker exec mcpr mcpr proxy list
docker exec mcpr mcpr proxy logs --tail 20
docker exec mcpr mcpr proxy status
```

## docker-compose

```yaml
services:
  mcpr:
    image: ghcr.io/pragmalabs-tech/mcpr:latest
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
- `X.Y.Z` — exact version (e.g. `0.5.0`)
- `X.Y` — latest patch of a minor line (e.g. `0.5`)
- `sha-<short>` — the commit hash (for reproducible pulls)

Pin to `X.Y.Z` in production. `latest` is for evaluation.

## Gotchas

- **`mcpr proxy run` takes the config file as a positional argument.** Both inside and outside the container, the form is `mcpr proxy run /path/mcpr.toml`. Omit the path to default to `mcpr.toml` in the working directory.
- **State lives at `/var/lib/mcpr/.mcpr/`, not `/var/lib/mcpr/`.** The `.mcpr` subdirectory is appended by mcpr. The volume mount point is the parent, not the state directory itself.
- **No `procps` in the image.** `docker exec ... ps` fails. Use `mcpr proxy list` and `mcpr proxy status` for process information.
