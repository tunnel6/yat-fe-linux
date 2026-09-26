# ── YAT Client Linux — Unified Build System ──────────────────────────────────
#
# Build matrix:
#   musl (Alpine 3.20)     : amd64  arm64
#   gnu  (Debian bookworm) : amd64  arm64  armv7
#
# Filter params (default: all):
#   LIBC=musl|gnu    ARCH=amd64|arm64|armv7
#
# Examples:
#   make build                              # all 5 targets
#   make build LIBC=musl                    # musl: amd64 + arm64
#   make build ARCH=armv7                   # gnu-armv7 only
#   make build LIBC=gnu ARCH=amd64          # gnu-amd64 only
#   make docker                             # all 5 images
#   make docker LIBC=musl                   # musl images only
#   make push TAG=v1.0.0                    # all + manifests
#   make push LIBC=gnu TAG=v1.0.0           # gnu + gnu manifest
#   make release TAG=v1.0.0                 # build + verify + push
#
# Shorthand:
#   make build-musl  =  make build LIBC=musl
#   make docker-gnu  =  make docker LIBC=gnu
#   make push-musl   =  make push LIBC=musl

.PHONY: all help \
        build build-musl build-gnu \
        verify verify-musl verify-gnu \
        stage stage-musl stage-gnu \
        docker docker-musl docker-gnu \
        push push-musl push-gnu \
        release ensure-buildkit clean

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

# ── Docker image matrix ─────────────────────────────────────────────────────
#                         musl (Alpine)     gnu (Debian)
#   amd64  ✓               ✓
#   arm64  ✓               ✓
#   armv7  —               ✓

DX_musl := musl-amd64 musl-arm64
DX_gnu  := gnu-amd64 gnu-arm64 gnu-armv7

BASE_musl-amd64  := $(ALIYUN_REG)/alpine:3.20-amd64
BASE_musl-arm64  := $(ALIYUN_REG)/alpine:3.20-arm64
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
DX_ACTIVE   := $(filter $(_libc_pat),$(filter $(_arch_pat),$(DX_musl) $(DX_gnu)))

# Buildkit: pull from Aliyun and tag locally so buildx never hits Docker Hub
BUILDKIT_SRC := $(ALIYUN_REG)/buildkit:buildx-stable-1
BUILDKIT_TAG := moby/buildkit:buildx-stable-1

# ── Default ──────────────────────────────────────────────────────────────────
all: help

help:
	@echo "Build matrix:"
	@echo "  musl (Alpine) : amd64  arm64"
	@echo "  gnu  (Debian) : amd64  arm64  armv7"
	@echo ""
	@echo "Params:  LIBC=musl|gnu  ARCH=amd64|arm64|armv7  (default: all)"
	@echo ""
	@echo "  build|verify|stage|docker|push [LIBC=..] [ARCH=..]"
	@echo "  build-musl|build-gnu|docker-musl|docker-gnu|push-musl|push-gnu"
	@echo "  release TAG=.. | clean"

# ══════════════════════════════════════════════════════════════════════════════
# Per-libc shorthands (recursive make with LIBC pre-set)
# ══════════════════════════════════════════════════════════════════════════════

build-musl:  ; @$(MAKE) build  LIBC=musl  ARCH=$(ARCH)  TAG=$(TAG)
build-gnu:   ; @$(MAKE) build  LIBC=gnu   ARCH=$(ARCH)  TAG=$(TAG)
verify-musl: ; @$(MAKE) verify LIBC=musl  ARCH=$(ARCH)  TAG=$(TAG)
verify-gnu:  ; @$(MAKE) verify LIBC=gnu   ARCH=$(ARCH)  TAG=$(TAG)
stage-musl:  ; @$(MAKE) stage  LIBC=musl  ARCH=$(ARCH)  TAG=$(TAG)
stage-gnu:   ; @$(MAKE) stage  LIBC=gnu   ARCH=$(ARCH)  TAG=$(TAG)
docker-musl: ; @$(MAKE) docker LIBC=musl  ARCH=$(ARCH)  TAG=$(TAG)
docker-gnu:  ; @$(MAKE) docker LIBC=gnu   ARCH=$(ARCH)  TAG=$(TAG)
push-musl:   ; @$(MAKE) push   LIBC=musl  ARCH=$(ARCH)  TAG=$(TAG)
push-gnu:    ; @$(MAKE) push   LIBC=gnu   ARCH=$(ARCH)  TAG=$(TAG)

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

docker: stage ensure-buildkit
	@docker buildx inspect yat-builder >/dev/null 2>&1 || \
		docker buildx create --name yat-builder --driver docker-container --use
	@docker buildx use yat-builder 2>/dev/null
	@$(foreach t,$(DX_ACTIVE), \
		libc=$(firstword $(subst -,$(sp),$(t))); arch=$(lastword $(subst -,$(sp),$(t))); \
		if [ "$$arch" = "armv7" ]; then plat=linux/arm/v7; else plat=linux/$$arch; fi; \
		if [ "$$arch" = "armv7" ]; then dx=arm; else dx=$$arch; fi; \
		base="$(BASE_$(t))"; \
		tags="-t $(GHCR):latest-$$libc-$$dx -t $(ALIYUN):latest-$$libc-$$dx"; \
		if [ "$(TAG)" != "latest" ]; then \
			tags="$$tags -t $(GHCR):$(TAG)-$$libc-$$dx -t $(ALIYUN):$(TAG)-$$libc-$$dx"; \
		fi; \
		echo ">> buildx build $$plat VARIANT=$$libc BASE=$$base"; \
		eval docker buildx build --load --pull --provenance=false --platform "$$plat" \
			--build-arg VARIANT=$$libc --build-arg BASE_IMAGE=$$base \
			$$tags -f Dockerfile.local2 .; \
	)

# ══════════════════════════════════════════════════════════════════════════════
# Push (buildx --push + multi-arch manifest)
# ══════════════════════════════════════════════════════════════════════════════

push: stage ensure-buildkit
	@docker buildx inspect yat-builder >/dev/null 2>&1 || \
		docker buildx create --name yat-builder --driver docker-container --use
	@docker buildx use yat-builder 2>/dev/null
	@$(foreach t,$(DX_ACTIVE), \
		libc=$(firstword $(subst -,$(sp),$(t))); arch=$(lastword $(subst -,$(sp),$(t))); \
		if [ "$$arch" = "armv7" ]; then plat=linux/arm/v7; else plat=linux/$$arch; fi; \
		if [ "$$arch" = "armv7" ]; then dx=arm; else dx=$$arch; fi; \
		base="$(BASE_$(t))"; \
		ver_tag=""; \
		if [ "$(TAG)" != "latest" ]; then \
			ver_tag="-t $(GHCR):$(TAG)-$$libc-$$dx -t $(ALIYUN):$(TAG)-$$libc-$$dx"; \
		fi; \
		echo ">> buildx push $$plat VARIANT=$$libc BASE=$$base"; \
		eval docker buildx build --pull --provenance=false --platform "$$plat" --push \
			--build-arg VARIANT=$$libc --build-arg BASE_IMAGE=$$base \
			$$ver_tag \
			-t $(GHCR):latest-$$libc-$$dx \
			-t $(ALIYUN):latest-$$libc-$$dx \
			-f Dockerfile.local2 .; \
	)
	@$(foreach libc,musl gnu, \
		$(if $(filter $(libc)-%,$(DX_ACTIVE)), \
			echo "=== Manifest $(libc) ==="; \
			if [ "$(TAG)" != "latest" ]; then \
				arch_imgs=""; \
				$(foreach t,$(filter $(libc)-%,$(DX_ACTIVE)), \
					arch=$(lastword $(subst -,$(sp),$(t))); \
					if [ "$$arch" = "armv7" ]; then dx=arm; else dx=$$arch; fi; \
					arch_imgs="$$arch_imgs $(GHCR):$(TAG)-$(libc)-$$dx"; \
				) \
				docker buildx imagetools create -t "$(GHCR):$(TAG)-$(libc)" $$arch_imgs; \
			fi; \
			latest_imgs=""; \
			$(foreach t,$(filter $(libc)-%,$(DX_ACTIVE)), \
				arch=$(lastword $(subst -,$(sp),$(t))); \
				if [ "$$arch" = "armv7" ]; then dx=arm; else dx=$$arch; fi; \
				latest_imgs="$$latest_imgs $(GHCR):latest-$(libc)-$$dx"; \
			) \
			docker buildx imagetools create -t "$(GHCR):latest-$(libc)" $$latest_imgs; \
		) \
	)
	@$(if $(filter gnu-%,$(DX_ACTIVE)), \
		echo "=== Manifest all-platform ==="; \
		if [ "$(TAG)" != "latest" ]; then \
			ver_imgs=""; \
			$(foreach t,$(filter gnu-%,$(DX_ACTIVE)), \
				arch=$(lastword $(subst -,$(sp),$(t))); \
				if [ "$$arch" = "armv7" ]; then dx=arm; else dx=$$arch; fi; \
				ver_imgs="$$ver_imgs $(GHCR):$(TAG)-gnu-$$dx"; \
			) \
			docker buildx imagetools create -t "$(GHCR):$(TAG)" $$ver_imgs; \
		fi; \
		all_imgs=""; \
		$(foreach t,$(filter gnu-%,$(DX_ACTIVE)), \
			arch=$(lastword $(subst -,$(sp),$(t))); \
			if [ "$$arch" = "armv7" ]; then dx=arm; else dx=$$arch; fi; \
			all_imgs="$$all_imgs $(GHCR):latest-gnu-$$dx"; \
		) \
		docker buildx imagetools create -t "$(GHCR):latest" $$all_imgs; \
	)

# ══════════════════════════════════════════════════════════════════════════════
# Release / Buildkit / Clean
# ══════════════════════════════════════════════════════════════════════════════

release: build verify push

ensure-buildkit:
	@if ! docker image inspect $(BUILDKIT_TAG) >/dev/null 2>&1; then \
		echo ">> Pulling buildkit from Aliyun and tagging locally..."; \
		docker pull $(BUILDKIT_SRC); \
		docker tag $(BUILDKIT_SRC) $(BUILDKIT_TAG); \
	fi

clean:
	cargo clean
	rm -rf $(STAGE_DIR)
	@echo "Cleaned target/ and $(STAGE_DIR)/"
