#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
# TERM-001：可复跑的 VT 兼容套件（vttest/esctest 风格）。
#
# 退出码 0 = 无 FAIL（XFAIL 是已记录的已知偏差，不计失败）。
# 需要可用的离线 cargo 缓存，例如：
#   CARGO_HOME=/path/to/rusttabbyterminal/.cargo ./scripts/vt_compat_suite.sh
set -euo pipefail

cd "$(dirname "$0")/.."
export CARGO_HOME="${CARGO_HOME:-$PWD/.cargo}"
export CARGO_NET_OFFLINE=true

exec cargo test -p xtervo-core --test vt_compat_suite --offline -- --report "$@"
