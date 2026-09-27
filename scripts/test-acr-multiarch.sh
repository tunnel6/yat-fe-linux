#!/usr/bin/env bash
# ── 测试阿里云 ACR 多架构 manifest 支持度 ────────────────────────────────────
#
# 用法: bash scripts/test-acr-multiarch.sh
#
# 测试项:
#   1. 检查阿里云上已有的多架构 manifest 是否包含正确平台
#   2. 尝试用 buildx imagetools create 在阿里云上创建多架构 manifest
#   3. 验证创建的 manifest 是否能正确服务各平台

set -euo pipefail

ALIYUN_REG="registry.cn-beijing.aliyuncs.com/pei"
TEST_IMG="$ALIYUN_REG/yat-client-linux"
PASS=0
FAIL=0

green() { printf '\033[32m%s\033[0m\n' "$*"; }
red()   { printf '\033[31m%s\033[0m\n' "$*"; }
bold()  { printf '\033[1m%s\033[0m\n' "$*"; }

# ── Test 1: 检查已有 base image 的多架构 manifest ────────────────────────────
bold "═══ Test 1: 检查阿里云 base image 多架构 manifest ═══"

for img in "$ALIYUN_REG/alpine:3.20" "$ALIYUN_REG/debian:bookworm-slim"; do
    echo ""
    bold ">>> inspect $img"
    if output=$(docker buildx imagetools inspect "$img" 2>&1); then
        echo "$output"
        unknown_count=$(echo "$output" | grep -c "unknown/unknown" || true)
        if [ "$unknown_count" -gt 0 ]; then
            red "  ✗ 包含 $unknown_count 个 unknown/unknown 条目"
            FAIL=$((FAIL + 1))
        else
            green "  ✓ 无 unknown/unknown 条目"
            PASS=$((PASS + 1))
        fi
    else
        red "  ✗ inspect 失败: $output"
        FAIL=$((FAIL + 1))
    fi
done

# ── Test 2: 检查单架构 tag 是否正常 ──────────────────────────────────────────
bold ""
bold "═══ Test 2: 检查单架构 base image tag ═══"

for img in \
    "$ALIYUN_REG/alpine:3.20-amd64" \
    "$ALIYUN_REG/alpine:3.20-arm64" \
    "$ALIYUN_REG/debian:bookworm-slim-amd64" \
    "$ALIYUN_REG/debian:bookworm-slim-arm64" \
    "$ALIYUN_REG/debian:bookworm-slim-armv7"; do
    echo ""
    bold ">>> inspect $img"
    if output=$(docker buildx imagetools inspect "$img" 2>&1); then
        echo "$output" | head -5
        green "  ✓ 可访问"
        PASS=$((PASS + 1))
    else
        red "  ✗ 不可访问: $output"
        FAIL=$((FAIL + 1))
    fi
done

# ── Test 3: 尝试在阿里云上创建多架构 manifest ────────────────────────────────
bold ""
bold "═══ Test 3: 在阿里云上创建测试 manifest ═══"
echo ""
echo "将 3 个单架构镜像合并为 yat-client-linux:multiarch-test"

# 收集已有的单架构镜像（检查是否存在）
src_imgs=""
for suffix in amd64 arm64 arm; do
    # 尝试找到对应的版本化 tag，如果不存在就跳过
    for tag in "latest-$suffix" "$suffix"; do
        if docker buildx imagetools inspect "$TEST_IMG:$tag" >/dev/null 2>&1; then
            src_imgs="$src_imgs $TEST_IMG:$tag"
            echo "  找到源镜像: $TEST_IMG:$tag"
            break
        fi
    done
done

if [ -z "$src_imgs" ]; then
    echo "  没有找到可用的单架构源镜像，跳过 manifest 创建测试"
    echo "  提示: 先运行 make push TAG=test 推送一些镜像"
else
    echo ""
    bold ">>> docker buildx imagetools create -t $TEST_IMG:multiarch-test $src_imgs"
    if output=$(docker buildx imagetools create -t "$TEST_IMG:multiarch-test" $src_imgs 2>&1); then
        green "  ✓ manifest 创建成功"
        PASS=$((PASS + 1))

        echo ""
        bold ">>> 验证创建的 manifest:"
        if inspect_out=$(docker buildx imagetools inspect "$TEST_IMG:multiarch-test" 2>&1); then
            echo "$inspect_out"
            unknown_count=$(echo "$inspect_out" | grep -c "unknown/unknown" || true)
            platform_count=$(echo "$inspect_out" | grep -c "linux/" || true)

            if [ "$unknown_count" -gt 0 ]; then
                red "  ✗ manifest 包含 $unknown_count 个 unknown/unknown 条目"
                FAIL=$((FAIL + 1))
            else
                green "  ✓ 无 unknown/unknown 条目"
                PASS=$((PASS + 1))
            fi

            if [ "$platform_count" -ge 2 ]; then
                green "  ✓ 包含 $platform_count 个有效平台"
                PASS=$((PASS + 1))
            else
                red "  ✗ 仅包含 $platform_count 个平台（期望 >= 2）"
                FAIL=$((FAIL + 1))
            fi
        else
            red "  ✗ inspect 失败: $inspect_out"
            FAIL=$((FAIL + 1))
        fi

        # 尝试拉取各平台
        echo ""
        bold ">>> 测试各平台拉取:"
        for plat in linux/amd64 linux/arm64 linux/arm/v7; do
            echo -n "  $plat: "
            if docker pull --platform "$plat" "$TEST_IMG:multiarch-test" >/dev/null 2>&1; then
                green "✓ 拉取成功"
                PASS=$((PASS + 1))
            else
                red "✗ 拉取失败"
                FAIL=$((FAIL + 1))
            fi
        done

        # 清理测试 manifest
        echo ""
        echo "清理测试 manifest..."
        # 注意: ACR 可能不支持删除 manifest，这里只做提示
        echo "  手动清理: docker buildx imagetools create -t $TEST_IMG:multiarch-test --dry-run"
        echo "  或在阿里云控制台删除 yat-client-linux:multiarch-test"
    else
        red "  ✗ manifest 创建失败: $output"
        FAIL=$((FAIL + 1))
    fi
fi

# ── 汇总 ─────────────────────────────────────────────────────────────────────
bold ""
bold "═══ 测试汇总 ═══"
green "  通过: $PASS"
[ "$FAIL" -gt 0 ] && red "  失败: $FAIL" || green "  失败: $FAIL"
echo ""
if [ "$FAIL" -eq 0 ]; then
    green "结论: 阿里云 ACR 多架构 manifest 支持正常，可以考虑切换！"
else
    red "结论: 阿里云 ACR 仍有问题，建议维持当前单架构 tag 方案。"
fi
