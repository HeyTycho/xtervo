#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
# TERM-002：确定性 VT fuzz 目标（不依赖 cargo-fuzz）。
#
# 默认是 CI 快速模式；soak/夜间用：
#   SEEDS=16 ITERS=100000 ./scripts/fuzz_vt.sh
# 任一断言（panic/越界/内存/确定性）失败都会以非 0 退出。
set -euo pipefail

cd "$(dirname "$0")/.."
export CARGO_HOME="${CARGO_HOME:-$PWD/.cargo}"
export CARGO_NET_OFFLINE=true

SEED="${SEED:-1}"
SEEDS="${SEEDS:-3}"
ITERS="${ITERS:-600}"
SCROLLBACK="${SCROLLBACK:-500}"

exec cargo test -p tervo-core --test vt_fuzz_target --offline -- \
    --seed "$SEED" --seeds "$SEEDS" --iters "$ITERS" --scrollback "$SCROLLBACK" "$@"
