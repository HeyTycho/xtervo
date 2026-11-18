#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
#
# Tervo 防复发 CI gate：不启动用户窗口，不需要 DISPLAY（e2e 自己起 Xvfb）。
# 适用：提交前/合并前。GPU 显示器 e2e 需要 adapter，单独在发布候选跑。
set -euo pipefail

ROOT=$(cd "$(dirname "$0")/.." && pwd)
cd "$ROOT"
export CARGO_HOME=${CARGO_HOME:-$ROOT/.cargo}
export CARGO_NET_OFFLINE=${CARGO_NET_OFFLINE:-true}

echo "[ci] core"
cargo test -p tervo-core

echo "[ci] app bin"
cargo test -p tervo-app --bin tervo

echo "[ci] app integration"
cargo test -p tervo-app --tests

echo "[ci] diagnostics"
cargo test -p tervo-diagnostics

echo "[ci] image scroll clip e2e (CPU deterministic)"
TERVO_CPU=1 bash scripts/e2e_image_scroll_clip.sh

echo "[ci] tabbar +/profile input focus e2e"
# 防复发：Profile 菜单曾用 FocusScope 抢焦点，导致菜单关闭后终端收不到按键。
# 该 e2e 必须包含“菜单打开时按普通键 → 终端仍收到 [key]”的断言。
cargo build -p tervo-app --bin tervo --bin xwin --bin xclick --bin xkey
TERVO_BIN="$ROOT/target/debug/tervo" bash scripts/e2e_tabbar_plus_profile.sh

echo "[ci] maximize isolated (release binary)"
cargo build --release -p tervo-app --bin tervo
BIN="$ROOT/target/release/tervo" bash scripts/e2e_maximize_isolated.sh

echo "[ci] PASS"
