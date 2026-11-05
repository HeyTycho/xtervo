#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
# 在**真机（有 GPU）**上验证 Slint Skia 后端的关键指标。
#
# 为什么需要单独跑：开发沙箱没有 /dev/dri，Skia 的 wgpu/GL 上下文创建后拿不到
# 可呈现的 surface，窗口内容为空。软件后端能跑通全部逻辑，但无法回答
# 「Skia 上传 + 合成 60FPS 是否达标」这个问题。
#
# 用法（在桌面会话里）：
#     ./scripts/run_gpu_validation.sh
# 产出：docs/gpu-{startup,latency,fps}.json 以及一帧截图（可选）。
set -euo pipefail
cd "$(dirname "$0")/.."

export CARGO_HOME="${CARGO_HOME:-$PWD/.cargo}"
export SLINT_BACKEND=winit-skia
mkdir -p docs

BIN=target/release/tervo
cargo build --release -p tervo-app --bin tervo

echo "== GPU 后端冒烟：能否在 5s 内产生第一次 Damage =="
if ! timeout 30 "$BIN" --cols 120 --rows 32 --exit-after-first 2>/dev/null | tee docs/gpu-first-frame.txt | grep -q "FIRST_DAMAGE_US"; then
    echo "!! Skia 后端未能上屏，请检查 GPU/驱动；下面的数字不可信。" >&2
    exit 1
fi

echo "== 延迟 =="
"$BIN" --cols 120 --rows 32 --latency 200 | grep -E '^\{' | tee docs/gpu-latency.json

echo "== 60FPS =="
"$BIN" --cols 120 --rows 32 --fps 8 | grep -E '^\{' | tee docs/gpu-fps.json

echo "完成。把 docs/gpu-*.json 回填到 docs/phase0-report.md。"
