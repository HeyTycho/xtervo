#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
# Phase 0 一键回归：单元/集成测试 + criterion 基准。
set -euo pipefail
cd "$(dirname "$0")/.."

export CARGO_HOME="${CARGO_HOME:-$PWD/.cargo}"
mkdir -p docs

echo "== 测试 =="
cargo test -p xtervo-core

echo "== 基准（criterion）=="
cargo bench -p xtervo-core --bench pty_throughput -- --noplot | tee docs/bench-output.txt

echo "完成：结果见 docs/bench-output.txt 与 target/criterion/"
