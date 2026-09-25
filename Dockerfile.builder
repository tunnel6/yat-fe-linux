# syntax=docker/dockerfile:1
# ── YAT CI Builder Image ─────────────────────────────────────────────────────
# Pre-installs all build tools for cross-compilation.
# Build:  docker build -f Dockerfile.builder -t registry.cn-beijing.aliyuncs.com/pei/yat-fe-linux:builder .
# Push:   docker push registry.cn-beijing.aliyuncs.com/pei/yat-fe-linux:builder
#
FROM rust:1.98

ARG ZIG_VERSION=0.16.0
ARG CARGO_ZIGBUILD_VERSION=0.22.2

# ── Debian apt mirror (CN) ───────────────────────────────────────────────────
RUN apt-get update && \
    DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
      binutils clang lld openssh-client ca-certificates curl xz-utils && \
    rm -rf /var/lib/apt/lists/*

# ── Zig + wrapper (strip unsupported aarch64 linker flags) ───────────────────
RUN curl -fsSL "https://ziglang.org/download/${ZIG_VERSION}/zig-linux-x86_64-${ZIG_VERSION}.tar.xz" \
      | tar -xJ -C /opt && \
    REAL_ZIG=/opt/zig-linux-x86_64-${ZIG_VERSION}/zig && \
    printf '#!/bin/sh\nARGS=""\nfor arg in "$@"; do\n  case "$arg" in\n    --fix-cortex-a53-843419*) ;;\n    *) ARGS="$ARGS $arg" ;;\n  esac\ndone\nexec "%s" $ARGS\n' "$REAL_ZIG" \
    > /usr/local/bin/zig && \
    chmod +x /usr/local/bin/zig

# ── Rustup mirror (CN) + stable toolchain ───────────────────────────────────
ENV RUSTUP_DIST_SERVER=https://rsproxy.cn
ENV RUSTUP_UPDATE_ROOT=https://rsproxy.cn/rustup
RUN rustup toolchain install stable --profile minimal --no-self-update && \
    rustup default stable && \
    rustup target add x86_64-unknown-linux-gnu && \
    rustup target add x86_64-unknown-linux-musl && \
    rustup target add aarch64-unknown-linux-gnu && \
    rustup target add aarch64-unknown-linux-musl

# ── cargo-zigbuild ───────────────────────────────────────────────────────────
RUN cargo install cargo-zigbuild --locked --version ${CARGO_ZIGBUILD_VERSION} && \
    rm -rf /usr/local/cargo/registry/src /usr/local/cargo/registry/cache

# ── Verify ───────────────────────────────────────────────────────────────────
RUN rustc --version && cargo --version && zig version && cargo-zigbuild --version
