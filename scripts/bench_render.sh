#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
# 渲染 damage 可复现基准（P1-06）：同进程 legacy-full vs damage 对照。
#
# 用法：
#   ./scripts/bench_render.sh                 # release（推荐，数字才有意义）
#   PROFILE=debug ./scripts/bench_render.sh   # 快速冒烟
#   CARGO_TARGET_DIR=$PWD/.target-render ./scripts/bench_render.sh
#
# 场景：single_row / typing_burst / full_200x50 / scroll（每项 200–600 帧）。
# 输出 `[bench] scenario=...` 与 `[bench] summary ... speedup`，见 docs/PERFORMANCE.md。
set -euo pipefail
cd "$(dirname "$0")/.."

export CARGO_HOME="${CARGO_HOME:-$PWD/.cargo}"
PROFILE=${PROFILE:-release}
if [ "$PROFILE" = "release" ]; then
    cargo test --release --offline -p xtervo-app --bin xtervo render_damage_benchmark \
        -- --ignored --nocapture
else
    cargo test --offline -p xtervo-app --bin xtervo render_damage_benchmark \
        -- --ignored --nocapture
fi
