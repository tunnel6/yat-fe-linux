# ── Package pre-built binary into Docker image ───────────────────────────────
#
# Build matrix:
#   musl (Alpine 3.20)     : amd64  arm64
#   gnu  (Debian bookworm) : amd64  arm64  armv7
#
# Binary is staged by Makefile to docker-staging/<libc>/<arch>/yat-client
# (armv7 → staging dir is "arm" to match Docker TARGETARCH)
#
# Usage:
#   make docker TAG=v1.0.0          # local build (all variants)
#   make push TAG=v1.0.0            # multi-arch buildx + push

ARG VARIANT=musl
ARG BASE_IMAGE=scratch

# ── musl → Alpine ───────────────────────────────────────────────────────────
FROM ${BASE_IMAGE} AS musl-base
RUN apk add --no-cache ca-certificates wireguard-tools iproute2 net-tools
    

# ── gnu → Debian slim (glibc) ────────────────────────────────────────────────
FROM ${BASE_IMAGE} AS gnu-base
RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates wireguard-tools iproute2  net-tools && \
    apt-get autoremove -y && \
    rm -rf /var/lib/apt/lists/*
ENV TZ=Asia/Shanghai

# ── Final ────────────────────────────────────────────────────────────────────
ARG VARIANT
FROM ${VARIANT}-base

ARG VARIANT=musl
ARG TARGETARCH

COPY docker-staging/${VARIANT}/${TARGETARCH}/yat-client /usr/local/bin/yat-client

# Run the client directly as PID 1. Docker stop reaches the owner notifier;
# the default foreground serve mode includes the Web management listener.
STOPSIGNAL SIGTERM
ENTRYPOINT ["/usr/local/bin/yat-client"]
CMD ["serve"]
