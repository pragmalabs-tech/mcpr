# syntax=docker/dockerfile:1.7
FROM debian:trixie-slim

ARG TARGETARCH
ARG VERSION

# ca-certificates: TLS to upstream MCP servers and cloud sync.
# curl: fetches the release binary at image build time.
# tini: PID 1, forwards SIGTERM and reaps zombies from double-forked proxies.
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates curl tini \
 && rm -rf /var/lib/apt/lists/*

RUN ARCH=$(case "$TARGETARCH" in amd64) echo x86_64;; arm64) echo aarch64;; *) echo "$TARGETARCH";; esac) \
 && curl -fsSL "https://github.com/pragmalabs-tech/mcpr/releases/download/${VERSION}/mcpr-${VERSION}-${ARCH}-unknown-linux-gnu.tar.gz" \
    | tar -xz -C /usr/local/bin/ \
 && chmod +x /usr/local/bin/mcpr

RUN groupadd --system --gid 10001 mcpr \
 && useradd  --system --uid 10001 --gid mcpr --home-dir /var/lib/mcpr --shell /usr/sbin/nologin mcpr \
 && mkdir -p /etc/mcpr /var/lib/mcpr \
 && chown -R mcpr:mcpr /var/lib/mcpr

COPY --chmod=0755 docker/entrypoint.sh /usr/local/bin/docker-entrypoint.sh

# HOME drives the default state directory (~/.mcpr/ → /var/lib/mcpr/.mcpr/).
# MCPR_NO_TUI disables the interactive TUI in non-tty environments.
ENV HOME=/var/lib/mcpr \
    MCPR_NO_TUI=1

VOLUME ["/var/lib/mcpr"]

# 3000: default proxy listen port (override in mcpr.toml).
EXPOSE 3000

USER mcpr
WORKDIR /var/lib/mcpr

ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/docker-entrypoint.sh"]

LABEL org.opencontainers.image.source="https://github.com/pragmalabs-tech/mcpr" \
      org.opencontainers.image.description="Observability-first proxy for MCP servers" \
      org.opencontainers.image.licenses="Apache-2.0"
