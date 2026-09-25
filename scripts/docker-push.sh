#!/usr/bin/env bash
set -euo pipefail

# ── Multi-arch build & push script ───────────────────────────────────────────
# Builds yat-client for amd64 + arm64, pushes to GHCR and Aliyun Registry.
#
# Prerequisites:
#   - Build binaries first:
#       cargo build --release --bin yat-client --target x86_64-unknown-linux-musl
#       cargo build --release --bin yat-client --target aarch64-unknown-linux-musl
#   - docker login ghcr.io
#   - docker login registry.cn-beijing.aliyuncs.com
#
# Usage:
#   ./scripts/docker-push.sh              # push :latest
#   ./scripts/docker-push.sh v1.0.0       # push :v1.0.0
#   ./scripts/docker-push.sh beta-abc123  # push :beta-abc123
#   ./scripts/docker-push.sh v1.0.0 --no-cache  # build without cache

TAG="${1:-latest}"
NO_CACHE="${2:-}"

GHCR="ghcr.io/tunnel6/yat-client-linux"
ALIYUN="registry.cn-beijing.aliyuncs.com/pei/yat-client-linux"

echo "=== Building yat-client ${TAG} (amd64 + arm64) ==="

# ── Build amd64 ──────────────────────────────────────────────────────────────
echo ">> Building amd64..."
docker build $NO_CACHE -f Dockerfile.local \
  --build-arg RUST_TARGET=x86_64-unknown-linux-musl \
  -t "${GHCR}:${TAG}-amd64" \
  -t "${ALIYUN}:${TAG}-amd64" \
  .

# ── Build arm64 ──────────────────────────────────────────────────────────────
echo ">> Building arm64..."
docker build $NO_CACHE -f Dockerfile.local \
  --build-arg RUST_TARGET=aarch64-unknown-linux-musl \
  -t "${GHCR}:${TAG}-arm64" \
  -t "${ALIYUN}:${TAG}-arm64" \
  .

# ── Push arch-specific tags ──────────────────────────────────────────────────
echo ">> Pushing arch-specific tags..."
docker push "${GHCR}:${TAG}-amd64"
docker push "${GHCR}:${TAG}-arm64"
docker push "${ALIYUN}:${TAG}-amd64"
docker push "${ALIYUN}:${TAG}-arm64"

# ── Create & push multi-arch manifest ────────────────────────────────────────
echo ">> Creating multi-arch manifest..."

for REG in "${GHCR}" "${ALIYUN}"; do
  echo ">> Manifest for ${REG}:${TAG}"
  docker manifest create "${REG}:${TAG}" \
    "${REG}:${TAG}-amd64" \
    "${REG}:${TAG}-arm64"
  docker manifest push "${REG}:${TAG}"
done

echo ""
echo "=== Done ==="
echo "  ${GHCR}:${TAG}"
echo "  ${ALIYUN}:${TAG}"
