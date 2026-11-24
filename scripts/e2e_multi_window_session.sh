#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
#
# 多窗口 session v2 黑盒验收（单进程多窗口 + 真实 WM）：
#   - 开 2 个窗口、不同 tabs/active_tab/geometry，窗口 2 最大化
#   - 等 2s autosave，断言 session.json 是 v2 且含两个窗口的布局
#   - kill -9 primary，重启按 v2 恢复两个窗口的 tabs/active_tab/geometry/maximized
#   - primary 关闭后 second 仍存活：autosave 继续写盘、config watcher 继续热更新
#   - --restore 显式恢复多窗口；v1 迁移；空/部分窗口回退不整机失败
#
# 只启动/回收本脚本自己的 Xvfb/openbox/app；display 被占用时直接失败。
# 用法：./scripts/e2e_multi_window_session.sh
#      E2E_OUT=target/mw-session-e2e E2E_DISPLAY=:153 ./scripts/e2e_multi_window_session.sh

set -u

ROOT=$(cd "$(dirname "$0")/.." && pwd)
OUT=${E2E_OUT:-$ROOT/target/mw-session-e2e}
DISPLAY_NUM=${E2E_DISPLAY:-:153}
SCREEN=${E2E_SCREEN:-1600x1000x24}
TARGET_DIR=${CARGO_TARGET_DIR:-$ROOT/target}
BIN=${XTERVO_BIN:-$TARGET_DIR/debug/xtervo}
XWIN=${E2E_XWIN:-$TARGET_DIR/debug/xwin}

XVFB_PID=""
OPENBOX_PID=""

cleanup() {
    [ -n "$OPENBOX_PID" ] && kill "$OPENBOX_PID" 2>/dev/null
    [ -n "$XVFB_PID" ] && kill "$XVFB_PID" 2>/dev/null
}
trap cleanup EXIT

fail() { echo "FAIL: $*" >&2; exit 1; }
pass() { echo "PASS: $*"; }

[ -x "$BIN" ] || fail "缺少 $BIN；先 cargo build -p xtervo-app --bin xtervo --bin xwin"
[ -x "$XWIN" ] || fail "缺少 $XWIN"
command -v xdpyinfo >/dev/null || fail "缺少 xdpyinfo"
command -v xwininfo >/dev/null || fail "缺少 xwininfo"
command -v xprop >/dev/null || fail "缺少 xprop"

COMMON=$(git rev-parse --git-common-dir)
MAIN_ROOT=$(cd "$(dirname "$COMMON")" && pwd)
PERF=${XTERVO_PERF_DIR:-$MAIN_ROOT/.perf}
XVFB=${E2E_XVFB:-$PERF/xvfb/usr/bin/Xvfb}
[ -x "$XVFB" ] || XVFB=$(command -v Xvfb || true)
[ -x "$XVFB" ] || fail "找不到 Xvfb"

OPENBOX=${XTERVO_OPENBOX_BIN:-$PERF/openbox/usr/bin/openbox}
OPENBOX_LIB=$PERF/openbox/usr/lib/x86_64-linux-gnu
OPENBOX_DATA=$PERF/openbox/usr/share
if [ ! -x "$OPENBOX" ]; then
    fail "找不到 $OPENBOX（maximized 恢复断言需要真实 WM）"
fi

if DISPLAY="$DISPLAY_NUM" xdpyinfo >/dev/null 2>&1; then
    fail "$DISPLAY_NUM 已被占用；本脚本不会 kill 别人的 display"
fi

rm -rf "$OUT"
mkdir -p "$OUT" "$OUT/openbox-home/.config/openbox"
cat >"$OUT/openbox-home/.config/openbox/rc.xml" <<'XML'
<?xml version="1.0" encoding="UTF-8"?>
<openbox_config xmlns="http://openbox.org/3.4/rc">
  <theme><name>Clearlooks</name><titleLayout>NLIMC</titleLayout></theme>
  <desktops><number>1</number></desktops>
  <resize><drawContents>yes</drawContents></resize>
  <keyboard></keyboard>
  <mouse></mouse>
</openbox_config>
XML

"$XVFB" "$DISPLAY_NUM" -screen 0 "$SCREEN" -ac -nolisten tcp >"$OUT/xvfb.log" 2>&1 &
XVFB_PID=$!
for _ in $(seq 1 100); do
    DISPLAY="$DISPLAY_NUM" xdpyinfo >/dev/null 2>&1 && break
    sleep 0.05
done
DISPLAY="$DISPLAY_NUM" xdpyinfo >/dev/null 2>&1 || fail "Xvfb 启动失败（$OUT/xvfb.log）"

DISPLAY="$DISPLAY_NUM" HOME="$OUT/openbox-home" \
    XDG_DATA_DIRS="$OPENBOX_DATA:${XDG_DATA_DIRS:-/usr/local/share:/usr/share}" \
    XDG_CONFIG_DIRS="$PERF/openbox/etc/xdg:${XDG_CONFIG_DIRS:-/etc/xdg}" \
    LD_LIBRARY_PATH="$OPENBOX_LIB" \
    "$OPENBOX" --sm-disable >"$OUT/openbox.log" 2>&1 &
OPENBOX_PID=$!
for _ in $(seq 1 100); do
    DISPLAY="$DISPLAY_NUM" xprop -root _NET_SUPPORTING_WM_CHECK 2>/dev/null | grep -q "window id" && break
    sleep 0.05
done
DISPLAY="$DISPLAY_NUM" xprop -root _NET_SUPPORTING_WM_CHECK 2>/dev/null | grep -q "window id" \
    || fail "Openbox 未就绪（$OUT/openbox.log）"
pass "Xvfb + Openbox 就绪（$DISPLAY_NUM）"

python3 "$ROOT/scripts/mw_session_e2e.py" \
    --out "$OUT" --bin "$BIN" --xwin "$XWIN" --display "$DISPLAY_NUM" \
    || fail "session e2e 断言失败"

echo "evidence: $OUT"
echo "PASS multi-window session v2 e2e"
