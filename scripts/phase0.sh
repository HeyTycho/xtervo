#!/usr/bin/env bash
# Phase 0 完整性能采集：冷启动 / 空闲 RSS / 按键延迟 / 全屏 60FPS。
#
# 注意：本沙箱内没有 /dev/dri，Slint 的 Skia(OpenGL/WGPU) 后端拿不到 GPU 设备，
# 因此默认用 winit-software 后端采集。要在真机上验证 Skia 路径：
#     SLINT_BACKEND=winit-skia ./scripts/phase0.sh
set -euo pipefail
cd "$(dirname "$0")/.."

export CARGO_HOME="${CARGO_HOME:-$PWD/.cargo}"
export DISPLAY="${DISPLAY:-:1}"
export SLINT_BACKEND="${SLINT_BACKEND:-winit-software}"
mkdir -p docs

COLS="${COLS:-120}"
ROWS="${ROWS:-32}"
BIN=target/release/rusttabby-app

echo "== 构建 release =="
cargo build --release -p rusttabby-app --bin rusttabby-app

echo "== 资源基线（cold start / idle RSS / idle CPU）=="
python3 scripts/measure_startup.py --runs 5 --cols "$COLS" --rows "$ROWS" --backend "$SLINT_BACKEND" \
    | tee "docs/startup-${SLINT_BACKEND}.json"

echo "== 按键→屏幕延迟（XTest 注入 + X11 Damage 打点）=="
"$BIN" --cols "$COLS" --rows "$ROWS" --latency 200 \
    | grep -E '^\{' | tee "docs/latency-${SLINT_BACKEND}.json"

echo "== 全屏 60FPS 压测 =="
"$BIN" --cols "$COLS" --rows "$ROWS" --fps 8 \
    | grep -E '^\{' | tee "docs/fps-${SLINT_BACKEND}.json"

echo "完成：docs/{startup,latency,fps}-${SLINT_BACKEND}.json"
