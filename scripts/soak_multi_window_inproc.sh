#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
#
# 双窗口（同进程，默认路径）soak：时长由 SOAK_SECONDS 控制（默认 120）；两个 MainWindow + 两套 PTY 持续输出，
# 每 5s 采样 RSS/线程/FD，结束时要求两个窗口仍存活且资源无持续增长。
# 不设置 TERVO_MULTI_WINDOW_INPROC：验证产品默认的同进程路径。
# 合并前至少 15m，发布候选 30m；2m 只作 smoke。
set -u
ROOT=$(cd "$(dirname "$0")/.." && pwd)
OUT=${SOAK_OUT:-$ROOT/target/multi-window-soak}
DISPLAY_NUM=${SOAK_DISPLAY:-:152}
DURATION=${SOAK_SECONDS:-120}
TARGET_DIR=${CARGO_TARGET_DIR:-$ROOT/target}
BIN=${TERVO_BIN:-$TARGET_DIR/debug/tervo}
TOKEN="mw-soak-token"
# control.sock/live.sock 都受 Unix socket 108 字节限制；产物仍写 worktree，
# 只有运行时 socket 放短的 /tmp。目录名带 * 前缀限制，避免误删用户路径。
RUNTIME=${SOAK_RUNTIME_DIR:-/tmp/tervo-mw-soak-$$}
case "$RUNTIME" in
    /tmp/tervo-mw-soak-*) ;;
    *) echo "SOAK_RUNTIME_DIR must be under /tmp/tervo-mw-soak-*" >&2; exit 2 ;;
esac
XVFB_PID=""
APP_PID=""
cleanup() {
    [ -n "$APP_PID" ] && kill "$APP_PID" 2>/dev/null
    [ -n "$XVFB_PID" ] && kill "$XVFB_PID" 2>/dev/null
    rm -rf -- "$RUNTIME"
}
trap cleanup EXIT
fail() { echo "FAIL: $*" >&2; exit 1; }
[ -x "$BIN" ] || fail "缺少 $BIN"
if DISPLAY="$DISPLAY_NUM" xdpyinfo >/dev/null 2>&1; then
    fail "$DISPLAY_NUM 已被占用"
fi
COMMON=$(git rev-parse --git-common-dir)
MAIN_ROOT=$(cd "$(dirname "$COMMON")" && pwd)
PERF=${TERVO_PERF_DIR:-$MAIN_ROOT/.perf}
XVFB=${SOAK_XVFB:-$PERF/xvfb/usr/bin/Xvfb}
[ -x "$XVFB" ] || XVFB=$(command -v Xvfb || true)
[ -x "$XVFB" ] || fail "找不到 Xvfb"
rm -rf "$OUT"; rm -rf -- "$RUNTIME"
mkdir -p "$OUT/logs" "$OUT/config" "$RUNTIME"
chmod 700 "$RUNTIME"
# session v2 autosave 也纳入 soak：2s 写盘必须与两个窗口共存且不涨资源。
cat >"$OUT/cfg.toml" <<'TOML'
restore_session_on_startup = true
cols = 80
rows = 24
shell = "/bin/sh"
TOML
"$XVFB" "$DISPLAY_NUM" -screen 0 1280x800x24 -ac -nolisten tcp >"$OUT/xvfb.log" 2>&1 &
XVFB_PID=$!
for _ in $(seq 1 100); do DISPLAY="$DISPLAY_NUM" xdpyinfo >/dev/null 2>&1 && break; sleep 0.05; done
export DISPLAY="$DISPLAY_NUM" XDG_RUNTIME_DIR="$RUNTIME" XDG_CONFIG_HOME="$OUT/config" XDG_STATE_HOME="$OUT/state"
export TERVO_CONFIG="$OUT/cfg.toml" TERVO_LOG_DIR="$OUT/logs" TERVO_SESSION="$OUT/session.json" TERVO_PROFILES="$OUT/profiles.toml"
export TERVO_DIAG_SOCKET="$RUNTIME/live.sock" TERVO_DIAG_TOKEN="$TOKEN" TERVO_DIAG_ALLOW_WRITE=1 TERVO_DIAG_ALLOW_DESTRUCTIVE=1
export TERVO_MULTI_WINDOW_SECOND=1 SLINT_BACKEND=winit-software XMODIFIERS=@im=none
"$BIN" --cols 80 --rows 24 --shell /bin/sh >"$OUT/app.log" 2>&1 &
APP_PID=$!
for _ in $(seq 1 200); do [ -S "$RUNTIME/live.sock" ] && break; sleep 0.05; done
[ -S "$RUNTIME/live.sock" ] || fail "live socket 未就绪"
sleep 1
python3 "$ROOT/scripts/mw_runtime_soak.py" \
    --socket "$RUNTIME/live.sock" --token "$TOKEN" --pid "$APP_PID" \
    --seconds "$DURATION" --out "$OUT" || fail "soak 未通过"
echo "evidence: $OUT"
