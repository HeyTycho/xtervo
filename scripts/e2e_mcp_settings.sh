#!/usr/bin/env bash
# MCP 配置化/分组授权 e2e 包装：独占 Xvfb 上启动 Tervo，再跑 Python harness。
#
# 覆盖：默认 0600 socket + state token、MCP 默认只读、input/tabs/destructive
# 默认拒绝、热授权/收紧、master 关闭移除 socket、错误 token、启动关闭、
# 无 XDG_RUNTIME_DIR 的 state-dir fallback。
#
# 用法：
#   TERVO_BIN=$PWD/.target-scheduled-mcp/debug/tervo ./scripts/e2e_mcp_settings.sh
set -euo pipefail
ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$ROOT"
OUT=${E2E_OUT:-$ROOT/.logs/mcp-settings-e2e}
mkdir -p "$OUT" /tmp/.X11-unix
XVFB=${E2E_XVFB_BIN:-}
if [ -z "$XVFB" ]; then
  COMMON=$(git rev-parse --git-common-dir)
  MAIN_ROOT=$(cd "$(dirname "$COMMON")" && pwd)
  XVFB=$MAIN_ROOT/.perf/xvfb/usr/bin/Xvfb
fi
[ -x "$XVFB" ] || XVFB=$(command -v Xvfb || true)
[ -x "$XVFB" ] || { echo "Xvfb not found" >&2; exit 2; }
BIN=${TERVO_BIN:-$ROOT/target/release/tervo}
[ -x "$BIN" ] || { echo "TERVO_BIN not executable: $BIN" >&2; exit 2; }
DISPLAY_NUM=${E2E_DISPLAY:-:219}
if DISPLAY=$DISPLAY_NUM xdpyinfo >/dev/null 2>&1; then
  echo "display $DISPLAY_NUM already in use" >&2
  exit 2
fi
"$XVFB" "$DISPLAY_NUM" -screen 0 1280x800x24 -ac -nolisten tcp >"$OUT/xvfb.log" 2>&1 &
XP=$!
cleanup() {
  kill "$XP" 2>/dev/null || true
}
trap cleanup EXIT
for _ in $(seq 1 100); do DISPLAY=$DISPLAY_NUM xdpyinfo >/dev/null 2>&1 && break; sleep 0.05; done
DISPLAY=$DISPLAY_NUM E2E_OUT="$OUT" TERVO_BIN="$BIN" \
  python3 "$ROOT/scripts/e2e_mcp_settings.py"
