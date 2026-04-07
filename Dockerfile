FROM debian:trixie-slim

ARG TARGETARCH
ARG VERSION

RUN apt-get update && apt-get install -y ca-certificates curl && rm -rf /var/lib/apt/lists/*

RUN ARCH=$(case "$TARGETARCH" in amd64) echo x86_64;; arm64) echo aarch64;; *) echo "$TARGETARCH";; esac) && \
    curl -fsSL "https://github.com/cptrodgers/mcpr/releases/download/${VERSION}/mcpr-${VERSION}-${ARCH}-unknown-linux-gnu.tar.gz" | \
    tar -xz -C /usr/local/bin/

WORKDIR /app
ENV MCPR_NO_TUI=1
ENTRYPOINT ["mcpr"]
CMD ["run"]
