#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
#
# 单实例 + 同进程多窗口产品路径黑盒验收：
#   - 第一次 tervo 成为 primary，监听 $XDG_RUNTIME_DIR/tervo/control.sock
#   - 第二次普通 tervo 调用 / tervo --window 转发 open_window，ack 后退出 0
#   - 两个窗口属于同一个主进程（_NET_WM_PID）
#   - Ctrl+Shift+N 默认同进程新建窗口，不再 spawn 子进程
#   - 错误 token 明确拒绝且不开窗
#   - 关一个窗口只移除该窗口；关 primary 进程仍活；关最后一个才退出
#   - session running marker 只在 primary 存活期间存在，退出后清理
#
# 只启动/回收本脚本自己的 Xvfb/app；不设置 TERVO_MULTI_WINDOW_INPROC。
# 用法：./scripts/e2e_single_instance_multi_window.sh
#      E2E_OUT=target/single-instance-e2e E2E_DISPLAY=:153 ./scripts/e2e_single_instance_multi_window.sh

set -u

ROOT=$(cd "$(dirname "$0")/.." && pwd)
OUT=${E2E_OUT:-$ROOT/target/single-instance-e2e}
DISPLAY_NUM=${E2E_DISPLAY:-:153}
SCREEN=${E2E_SCREEN:-1280x800x24}
TARGET_DIR=${CARGO_TARGET_DIR:-$ROOT/target}
BIN=${TERVO_BIN:-$TARGET_DIR/debug/tervo}
XWIN=${E2E_XWIN:-$TARGET_DIR/debug/xwin}
XKEY=${E2E_XKEY:-$TARGET_DIR/debug/xkey}
TOKEN="instance-e2e-token"
# XDG_RUNTIME_DIR 会直接拼 control.sock；worktree 路径可能超过 Unix socket 的
# 108 字节上限，所以测试用短的 /tmp runtime（与真实 /run/user/<uid> 类似）。
RUNTIME=${E2E_RUNTIME_DIR:-/tmp/tervo-si-e2e-$$}
case "$RUNTIME" in
    /tmp/tervo-si-e2e-*) ;;
    *) echo "E2E_RUNTIME_DIR must be under /tmp/tervo-si-e2e-*" >&2; exit 2 ;;
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
pass() { echo "PASS: $*"; }

[ -x "$BIN" ] || fail "缺少 $BIN；先 cargo build -p tervo-app --bin tervo --bin xwin --bin xkey"
[ -x "$XWIN" ] || fail "缺少 $XWIN"
[ -x "$XKEY" ] || fail "缺少 $XKEY"
# 统一成无 `..` 的绝对路径：/proc/<pid>/cmdline 是启动时的字面路径。
BIN=$(readlink -f "$BIN")
XWIN=$(readlink -f "$XWIN")
XKEY=$(readlink -f "$XKEY")
command -v xdpyinfo >/dev/null || fail "缺少 xdpyinfo"
command -v xwininfo >/dev/null || fail "缺少 xwininfo"
command -v xprop >/dev/null || fail "缺少 xprop"

COMMON=$(git rev-parse --git-common-dir)
MAIN_ROOT=$(cd "$(dirname "$COMMON")" && pwd)
PERF=${TERVO_PERF_DIR:-$MAIN_ROOT/.perf}
XVFB=${E2E_XVFB:-$PERF/xvfb/usr/bin/Xvfb}
[ -x "$XVFB" ] || XVFB=$(command -v Xvfb || true)
[ -x "$XVFB" ] || fail "找不到 Xvfb"

if DISPLAY="$DISPLAY_NUM" xdpyinfo >/dev/null 2>&1; then
    fail "$DISPLAY_NUM 已被占用；本脚本不会 kill 别人的 display"
fi

rm -rf "$OUT"
rm -rf -- "$RUNTIME"
mkdir -p "$OUT/logs" "$OUT/config" "$OUT/state" "$RUNTIME"
chmod 700 "$RUNTIME"

"$XVFB" "$DISPLAY_NUM" -screen 0 "$SCREEN" -ac -nolisten tcp >"$OUT/xvfb.log" 2>&1 &
XVFB_PID=$!
for _ in $(seq 1 100); do
    DISPLAY="$DISPLAY_NUM" xdpyinfo >/dev/null 2>&1 && break
    sleep 0.05
done
DISPLAY="$DISPLAY_NUM" xdpyinfo >/dev/null 2>&1 || fail "Xvfb 启动失败（$OUT/xvfb.log）"

export DISPLAY="$DISPLAY_NUM"
export PATH="$(dirname "$XWIN"):$PATH"
export XDG_RUNTIME_DIR="$RUNTIME"
export XDG_CONFIG_HOME="$OUT/config"
export XDG_STATE_HOME="$OUT/state"
printf 'version = 1\nconfirm_close = false\n' > "$OUT/cfg.toml"
export TERVO_CONFIG="$OUT/cfg.toml"
export TERVO_LOG_DIR="$OUT/logs"
export TERVO_SESSION="$OUT/session.json"
export TERVO_PROFILES="$OUT/profiles.toml"
export TERVO_PLUGINS="$OUT/plugins"
export TERVO_DIAG_SOCKET="$RUNTIME/live.sock"
export TERVO_DIAG_TOKEN="$TOKEN"
export TERVO_DIAG_ALLOW_WRITE=1
export TERVO_DIAG_ALLOW_DESTRUCTIVE=1
export SLINT_BACKEND=winit-software
export XMODIFIERS=@im=none
# 默认路径必须自己工作：不要用 legacy/inproc env 掩盖回归。
unset TERVO_MULTI_WINDOW_INPROC TERVO_MULTI_WINDOW_LEGACY TERVO_MULTI_WINDOW_SECOND
unset TERVO_SINGLE_INSTANCE TERVO_INSTANCE_DIR

"$BIN" --cols 80 --rows 24 --shell /bin/sh >"$OUT/app.log" 2>&1 &
APP_PID=$!
for _ in $(seq 1 200); do
    [ -S "$RUNTIME/live.sock" ] && break
    sleep 0.05
done
[ -S "$RUNTIME/live.sock" ] || fail "live socket 未就绪（见 $OUT/app.log）"
[ -S "$RUNTIME/tervo/control.sock" ] || fail "control.sock 未就绪（见 $OUT/app.log）"
sleep 0.5

TERVO_BIN="$BIN" E2E_XWIN="$XWIN" E2E_XKEY="$XKEY" \
    python3 "$ROOT/scripts/single_instance_e2e.py" \
    --binary "$BIN" --pid "$APP_PID" --display "$DISPLAY_NUM" \
    --runtime-dir "$RUNTIME" --session "$TERVO_SESSION" --out "$OUT" \
    --app-log "$OUT/app.log" \
    || fail "single-instance e2e 断言失败"

# Ctrl+Shift+N / 关窗都已由 harness 验证；现在收集最后一个窗口关闭后的退出码。
wait "$APP_PID"
APP_STATUS=$?
APP_PID=""
[ "$APP_STATUS" -eq 0 ] || fail "关闭最后一个窗口后 primary 退出码=$APP_STATUS（应为 0）"
[ ! -e "$OUT/session.running" ] || fail "正常退出后 session running marker 仍存在"
[ ! -S "$RUNTIME/tervo/control.sock" ] || fail "正常退出后 control.sock 仍存在"

grep -E '\[instance\] primary|\[instance\] secondary|\[ui\] .* window created|close requested|last window' \
    "$OUT/app.log" >"$OUT/evidence.txt" || true
pass "single-instance multi-window e2e"
echo "evidence: $OUT"
