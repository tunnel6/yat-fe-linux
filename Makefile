# ── YAT Client Linux — Unified Build System ──────────────────────────────────
#
# Rust build matrix:
#   musl (Alpine 3.20)     : amd64  arm64          (local build only)
#   gnu  (Debian bookworm) : amd64  arm64  armv7   (Docker images)
#
# Docker images use gnu (Debian) only — musl binaries have segfault issues.
#
# Tag layout (TAG=latest):
#   Per-arch:   yat-client-linux:amd64 | arm64 | arm
#   Manifest:   yat-client-linux:latest  (amd64 + arm64 + arm)
#
# Tag layout (TAG=v1.0.0):
#   Per-arch:   yat-client-linux:amd64-v1.0.0 | arm64-v1.0.0 | arm-v1.0.0
#   Manifest:   yat-client-linux:v1.0.0  (amd64 + arm64 + arm)
#
# Filter params (default: all):
#   ARCH=amd64|arm64|armv7    (Docker uses gnu only)
#
# Examples:
#   make build                              # all 5 Rust targets
#   make build LIBC=musl                    # musl: amd64 + arm64
#   make build ARCH=armv7                   # gnu-armv7 only
#   make docker                             # 3 gnu images
#   make docker ARCH=arm64                  # gnu-arm64 only
#   make push TAG=v1.0.0                    # per-arch + manifest
#   make release TAG=v1.0.0                 # build + verify + push
#
# Shorthand:
#   make build-musl  =  make build LIBC=musl
#   make build-gnu   =  make build LIBC=gnu

.PHONY: all help \
        build build-musl build-gnu \
        verify verify-musl verify-gnu \
        stage stage-gnu \
        docker \
        push \
        release clean

# ── Configuration ────────────────────────────────────────────────────────────
empty :=
sp := $(empty) $(empty)
BIN      := yat-client
TAG      ?= latest
GHCR     := ghcr.io/tunnel6/yat-client-linux
ALIYUN   := registry.cn-beijing.aliyuncs.com/pei/yat-client-linux
ALIYUN_REG := registry.cn-beijing.aliyuncs.com/pei

# ── Rust target matrix ──────────────────────────────────────────────────────
#                         musl              gnu
#   amd64  ✓ (cargo)     ✓ (cargo)
#   arm64  ✓ (cross)     ✓ (cargo)
#   armv7  —             ✓ (cross)

ALL_RUST := musl-amd64 musl-arm64 gnu-amd64 gnu-arm64 gnu-armv7

RUST_musl-amd64  := x86_64-unknown-linux-musl
RUST_musl-arm64  := aarch64-unknown-linux-musl
RUST_gnu-amd64   := x86_64-unknown-linux-gnu
RUST_gnu-arm64   := aarch64-unknown-linux-gnu
RUST_gnu-armv7   := armv7-unknown-linux-gnueabihf

TOOL_musl-amd64  := cargo
TOOL_musl-arm64  := cross
TOOL_gnu-amd64   := cargo
TOOL_gnu-arm64   := cargo
TOOL_gnu-armv7   := cross

# ── Docker image matrix (gnu only) ───────────────────────────────────────────
#   gnu-amd64  gnu-arm64  gnu-armv7

DX_ALL := gnu-amd64 gnu-arm64 gnu-armv7

BASE_gnu-amd64   := $(ALIYUN_REG)/debian:bookworm-slim-amd64
BASE_gnu-arm64   := $(ALIYUN_REG)/debian:bookworm-slim-arm64
BASE_gnu-armv7   := $(ALIYUN_REG)/debian:bookworm-slim-armv7

# ── Active target filtering (LIBC / ARCH params) ────────────────────────────
ifeq ($(LIBC),)
  _libc_pat := %
else
  _libc_pat := $(LIBC)-%
endif

ifeq ($(ARCH),)
  _arch_pat := %
else
  _arch_pat := %-$(ARCH)
endif

RUST_ACTIVE := $(filter $(_libc_pat),$(filter $(_arch_pat),$(ALL_RUST)))
DX_ACTIVE   := $(filter $(_libc_pat),$(filter $(_arch_pat),$(DX_ALL)))

# ── Default ──────────────────────────────────────────────────────────────────
all: help

help:
	@echo "Rust build matrix:"
	@echo "  musl (Alpine) : amd64  arm64        (local build only)"
	@echo "  gnu  (Debian) : amd64  arm64  armv7 (Docker images)"
	@echo ""
	@echo "Docker: gnu only (musl binaries have segfault issues)"
	@echo ""
	@echo "Tag layout:"
	@echo "  TAG=latest   → :amd64 :arm64 :arm + :latest manifest"
	@echo "  TAG=v1.0.0   → :amd64-v1.0.0 :arm64-v1.0.0 :arm-v1.0.0 + :v1.0.0 manifest"
	@echo ""
	@echo "Params:  ARCH=amd64|arm64|armv7  (default: all)"
	@echo ""
	@echo "  build|verify|stage|docker|push [ARCH=..]"
	@echo "  build-musl|build-gnu"
	@echo "  release TAG=.. | clean"

# ══════════════════════════════════════════════════════════════════════════════
# Per-libc shorthands (recursive make with LIBC pre-set)
# ══════════════════════════════════════════════════════════════════════════════

build-musl:  ; @$(MAKE) build  LIBC=musl  ARCH=$(ARCH)  TAG=$(TAG)
build-gnu:   ; @$(MAKE) build  LIBC=gnu   ARCH=$(ARCH)  TAG=$(TAG)
verify-musl: ; @$(MAKE) verify LIBC=musl  ARCH=$(ARCH)  TAG=$(TAG)
verify-gnu:  ; @$(MAKE) verify LIBC=gnu   ARCH=$(ARCH)  TAG=$(TAG)
stage-gnu:   ; @$(MAKE) stage  LIBC=gnu   ARCH=$(ARCH)  TAG=$(TAG)

# ══════════════════════════════════════════════════════════════════════════════
# Build (Rust binaries)
# ══════════════════════════════════════════════════════════════════════════════

build: $(addprefix build-,$(RUST_ACTIVE))

$(addprefix build-,$(ALL_RUST)):
	$(TOOL_$(patsubst build-%,%,$@)) build --release --bin $(BIN) \
		--target $(RUST_$(patsubst build-%,%,$@))

# ══════════════════════════════════════════════════════════════════════════════
# Verify
# ══════════════════════════════════════════════════════════════════════════════

verify:
	@echo "=== Verify binaries ==="
	@$(foreach t,$(RUST_ACTIVE), \
		bin="target/$(RUST_$(t))/release/$(BIN)"; \
		if [ ! -f "$$bin" ]; then echo "ERROR: $$bin not found" >&2; exit 1; fi; \
		size=$$(stat -f%z "$$bin" 2>/dev/null || stat -c%s "$$bin"); \
		echo "  $(t): $$size bytes — $$(file -b "$$bin")"; \
	)

# ══════════════════════════════════════════════════════════════════════════════
# Stage (docker-staging/<libc>/<arch>/, armv7 → arm)
# ══════════════════════════════════════════════════════════════════════════════

STAGE_DIR := docker-staging

stage: verify
	@rm -rf $(STAGE_DIR)
	@$(foreach t,$(DX_ACTIVE), \
		arch=$(lastword $(subst -,$(sp),$(t))); \
		if [ "$$arch" = "armv7" ]; then dx=arm; else dx=$$arch; fi; \
		libc=$(firstword $(subst -,$(sp),$(t))); \
		mkdir -p $(STAGE_DIR)/$$libc/$$dx; \
		cp target/$(RUST_$(t))/release/$(BIN) $(STAGE_DIR)/$$libc/$$dx/$(BIN); \
	)
	@echo "Staged:" && find $(STAGE_DIR) -type f -exec ls -la {} \;

# ══════════════════════════════════════════════════════════════════════════════
# Docker (local build, --load)
# ══════════════════════════════════════════════════════════════════════════════

docker: stage ensure-builder
	@$(foreach t,$(DX_ACTIVE), \
		arch=$(lastword $(subst -,$(sp),$(t))); \
		if [ "$$arch" = "armv7" ]; then dx=arm; else dx=$$arch; fi; \
		if [ "$$arch" = "armv7" ]; then plat=linux/arm/v7; else plat=linux/$$arch; fi; \
		base="$(BASE_$(t))"; \
		tags="-t $(GHCR):$$dx -t $(ALIYUN):$$dx"; \
		if [ "$(TAG)" != "latest" ]; then \
			tags="$$tags -t $(GHCR):$$dx-$(TAG) -t $(ALIYUN):$$dx-$(TAG)"; \
		fi; \
		echo ">> buildx build $$plat BASE=$$base"; \
		eval docker buildx build --load --platform "$$plat" \
			--build-arg VARIANT=gnu --build-arg BASE_IMAGE=$$base \
			$$tags -f Dockerfile .; \
	)

# ══════════════════════════════════════════════════════════════════════════════
# Push (buildx --push + multi-arch manifest)
# ══════════════════════════════════════════════════════════════════════════════

push: stage ensure-builder
	@# ── Step 1: buildx build & push to GHCR only ──
	@$(foreach t,$(DX_ACTIVE), \
		arch=$(lastword $(subst -,$(sp),$(t))); \
		if [ "$$arch" = "armv7" ]; then dx=arm; else dx=$$arch; fi; \
		if [ "$$arch" = "armv7" ]; then plat=linux/arm/v7; else plat=linux/$$arch; fi; \
		base="$(BASE_$(t))"; \
		if [ "$(TAG)" = "latest" ]; then arch_tag=$$dx; else arch_tag=$$dx-$(TAG); fi; \
		echo ">> buildx push $$plat → GHCR BASE=$$base"; \
		eval docker buildx build --pull --provenance=false --platform "$$plat" --push \
			--build-arg VARIANT=gnu --build-arg BASE_IMAGE=$$base \
			-t $(GHCR):$$arch_tag \
			-f Dockerfile .; \
	)
	@# ── Step 2: skopeo copy GHCR → Aliyun (per-arch) ──
	@echo "=== skopeo sync GHCR → Aliyun ==="; \
	$(foreach t,$(DX_ACTIVE), \
		arch=$(lastword $(subst -,$(sp),$(t))); \
		if [ "$$arch" = "armv7" ]; then dx=arm; else dx=$$arch; fi; \
		if [ "$(TAG)" = "latest" ]; then arch_tag=$$dx; else arch_tag=$$dx-$(TAG); fi; \
		echo "  $(GHCR):$$arch_tag → $(ALIYUN):$$arch_tag"; \
		skopeo copy --all \
			docker://$(GHCR):$$arch_tag \
			docker://$(ALIYUN):$$arch_tag; \
	)
	@# ── Step 3: multi-arch manifest (both registries) ──
	@echo "=== Multi-arch manifest (GHCR): $(TAG) ==="; \
	ghcr_imgs=""; \
	aliyun_imgs=""; \
	$(foreach t,$(DX_ACTIVE), \
		arch=$(lastword $(subst -,$(sp),$(t))); \
		if [ "$$arch" = "armv7" ]; then dx=arm; else dx=$$arch; fi; \
		if [ "$(TAG)" = "latest" ]; then src_tag=$$dx; else src_tag=$$dx-$(TAG); fi; \
		ghcr_imgs="$$ghcr_imgs $(GHCR):$$src_tag"; \
		aliyun_imgs="$$aliyun_imgs $(ALIYUN):$$src_tag"; \
	) \
	docker buildx imagetools create -t "$(GHCR):$(TAG)" $$ghcr_imgs; \
	docker buildx imagetools create -t "$(ALIYUN):$(TAG)" $$aliyun_imgs; \
	if [ "$(TAG)" != "latest" ]; then \
		echo "=== Multi-arch manifest (GHCR): latest ==="; \
		docker buildx imagetools create -t "$(GHCR):latest" $$ghcr_imgs; \
		echo "=== Multi-arch manifest (Aliyun): latest ==="; \
		docker buildx imagetools create -t "$(ALIYUN):latest" $$aliyun_imgs; \
	fi

# ══════════════════════════════════════════════════════════════════════════════
# Release / Buildkit / Clean
# ══════════════════════════════════════════════════════════════════════════════

release: build verify push

ensure-builder:
	@docker buildx inspect yat-builder >/dev/null 2>&1 || \
		docker buildx create --name yat-builder --driver docker-container --use

clean:
	cargo clean
	rm -rf $(STAGE_DIR)
	@echo "Cleaned target/ and $(STAGE_DIR)/"
