#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
# TabBar 旧交互回归（LOCAL-004）：
#   `+` 直接新建默认 Tab（不弹 Profile 下拉）；`>_` 只做 Profile 选择，
#   菜单是纯列表（无搜索框），键盘 ↓ + Enter 可打开第二个 Profile。
#
# 用法：先构建
#   cargo build -p tervo-app --bin tervo --bin xwin --bin xclick --bin xkey --offline
#   ./scripts/e2e_tabbar_plus_profile.sh
# 依赖：Xvfb/Openbox（仓库 .perf/）、xwd、python3 + Pillow、xwin/xclick/xkey。
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT=$PWD
OUT=${TABBAR_E2E_OUT:-$ROOT/.logs/tabbar-plus-profile/e2e}
mkdir -p "$OUT/shots" "$OUT/home/.config/tervo" "$OUT/logs" "$OUT/instance" /tmp/.X11-unix
rm -f "$OUT"/shots/*.png "$OUT"/app.log

COMMON=$(git rev-parse --git-common-dir)
MAIN_ROOT=$(cd "$(dirname "$COMMON")" && pwd)
PERF=${TERVO_PERF_DIR:-$MAIN_ROOT/.perf}
XVFB_BIN=${E2E_XVFB_BIN:-$PERF/xvfb/usr/bin/Xvfb}
[ -x "$XVFB_BIN" ] || XVFB_BIN=$(command -v Xvfb || true)
[ -x "$XVFB_BIN" ] || { echo "找不到 Xvfb" >&2; exit 2; }
OPENBOX=$PERF/openbox/usr/bin/openbox
OPENBOX_LIB=$PERF/openbox/usr/lib/x86_64-linux-gnu

BIN=${TERVO_BIN:-$MAIN_ROOT/target/debug/tervo}
XWIN=${XWIN_BIN:-$MAIN_ROOT/target/debug/xwin}
XCLICK=${XCLICK_BIN:-$MAIN_ROOT/target/debug/xclick}
XKEY=${XKEY_BIN:-$MAIN_ROOT/target/debug/xkey}
for f in "$BIN" "$XWIN" "$XCLICK" "$XKEY"; do
    [ -x "$f" ] || { echo "缺少 $f（先 cargo build -p tervo-app --bin tervo --bin xwin --bin xclick --bin xkey）" >&2; exit 2; }
done

export DISPLAY=${E2E_DISPLAY:-:152}
export HOME=$OUT/home
export XMODIFIERS=@im=none
export SLINT_BACKEND=winit-software
if xdpyinfo >/dev/null 2>&1; then
    echo "display $DISPLAY 已被占用；先停掉占用它的会话。" >&2
    exit 2
fi

XVFB_PID=""
OB_PID=""
APP_PID=""
cleanup() {
    [ -n "$APP_PID" ] && kill "$APP_PID" 2>/dev/null || true
    [ -n "$OB_PID" ] && kill "$OB_PID" 2>/dev/null || true
    [ -n "$XVFB_PID" ] && kill "$XVFB_PID" 2>/dev/null || true
}
trap cleanup EXIT

"$XVFB_BIN" "$DISPLAY" -screen 0 1600x1000x24 -ac -nolisten tcp >"$OUT/xvfb.log" 2>&1 &
XVFB_PID=$!
for _ in $(seq 1 100); do xdpyinfo >/dev/null 2>&1 && break; sleep 0.05; done
xdpyinfo >/dev/null 2>&1 || { echo "Xvfb 启动失败：$OUT/xvfb.log" >&2; exit 2; }
if [ -x "$OPENBOX" ]; then
    LD_LIBRARY_PATH=$OPENBOX_LIB "$OPENBOX" >"$OUT/openbox.log" 2>&1 &
    OB_PID=$!
    sleep 0.6
fi

cat >"$OUT/home/.config/tervo/config.toml" <<'EOF'
version = 1
language = "en"
theme = "gruvbox-dark"
font_size = 15.0
confirm_close = false
restore_session_on_startup = false
EOF
cat >"$OUT/home/.config/tervo/profiles.toml" <<'EOF'
version = 2
next_id = 3

[[profiles]]
id = 1
name = "alpha"
kind = "local"
shell = "/bin/sh"

[[profiles]]
id = 2
name = "beta"
kind = "local"
shell = "/bin/sh"
EOF

TERVO_INSTANCE_DIR="$OUT/instance" TERVO_KEYLOG=1 \
    "$BIN" --cols 120 --rows 32 --shell /bin/sh >"$OUT/app.log" 2>&1 &
APP_PID=$!

WIN=""
for _ in $(seq 1 200); do
    WIN=$(xwininfo -root -tree 2>/dev/null | grep -i '"tervo"' | head -1 | awk '{print $1}' || true)
    [ -n "$WIN" ] && break
    sleep 0.05
done
[ -n "$WIN" ] || { echo "启动失败：$OUT/app.log" >&2; exit 2; }

refresh_geom() {
    ABS_X=$(xwininfo -id "$WIN" | awk '/Absolute upper-left X/{print $NF}')
    ABS_Y=$(xwininfo -id "$WIN" | awk '/Absolute upper-left Y/{print $NF}')
}
refresh_geom
"$XWIN" Tervo 20 20 >/dev/null 2>&1 || true
sleep 1
refresh_geom
if [ "$ABS_X" != 20 ] || [ "$ABS_Y" != 20 ]; then
    echo "窗口坐标与假设不符：${ABS_X},${ABS_Y}" >&2
    exit 2
fi

shot() {
    xwd -silent -id "$WIN" -out "$OUT/shots/$1.xwd"
    python3 "$MAIN_ROOT/scripts/xwd2png.py" "$OUT/shots/$1.xwd" "$OUT/shots/$1.png" >/dev/null
    rm -f "$OUT/shots/$1.xwd"
    echo "  shot $OUT/shots/$1.png"
}
click() { "$XCLICK" "$((ABS_X + $1))" "$((ABS_Y + $2))" >/dev/null; sleep "${3:-0.5}"; }
key() { "$XWIN" Tervo 20 20 >/dev/null 2>&1 || true; sleep 0.15; "$XKEY" "$@" >/dev/null; }
fail() { echo "FAIL: $*" >&2; exit 2; }

shot 01-start
grep -Fq '[ws] new tab: tabs=1' "$OUT/app.log" || fail "初始 Tab 数量不是 1"

echo "== 1. + 直接新建默认 Tab（不弹 Profile 菜单）=="
MENUS_BEFORE=$(grep -c '\[ui\] profile menu' "$OUT/app.log" || true)
click 187 16 0.8
grep -Fq '[ws] new tab: tabs=2' "$OUT/app.log" || fail "+ 没有直接新建 Tab"
MENUS_AFTER=$(grep -c '\[ui\] profile menu' "$OUT/app.log" || true)
[ "$MENUS_BEFORE" = "$MENUS_AFTER" ] || fail "+ 仍然打开了 Profile 菜单"
shot 02-plus-direct-new-tab

echo "== 2. 关掉新 Tab，回到单 Tab 后打开 >_ Profile 菜单 =="
key w --ctrl
sleep 0.8
grep -Fq '[ws] new tab: tabs=1' "$OUT/app.log" || fail "Ctrl+W 没回到 1 个 Tab"
click 212 16 0.8
grep -Fq '[ui] profile menu profiles=2' "$OUT/app.log" || fail ">_ 没有打开 Profile 菜单"
shot 03-profile-menu-no-search

echo "== 2b. 菜单打开时按普通键：关闭菜单且按键回到终端（不能丢焦点）=="
key "x"
sleep 0.4
grep -Fq '[key] "x"' "$OUT/app.log" || fail "菜单关闭后按键没有回到终端（焦点丢失）"
click 212 16 0.8
MENU_COUNT=$(grep -c '\[ui\] profile menu profiles=2' "$OUT/app.log" || true)
[ "$MENU_COUNT" -ge 2 ] || fail "重新打开 Profile 菜单失败"

echo "== 3. 键盘 ↓ + Enter 打开 beta =="
key --key down
sleep 0.3
shot 04-menu-keyboard-down
key --key enter
sleep 0.8
grep -Fq '[profile] open "beta"' "$OUT/app.log" || fail "键盘导航没有打开 beta"
shot 05-profile-beta-opened

echo "== 4. 3 Tab + 标题 spinner：点 Tab 必须切换，不能误关 =="
# 当前 2 个 Tab（原始 + beta）；再开一个，spinner 跑在第三个 Tab 里。
key t --ctrl
sleep 1.0
grep -Fq '[ws] new tab: tabs=3' "$OUT/app.log" || fail "没有到 3 个 Tab"
SPIN="$OUT/home/spin.py"
cat >"$SPIN" <<'PY'
import sys, time
i = 0
while True:
    i += 1
    sys.stdout.write("\033]0;spin-%s\007" % ("A" if i % 2 else "B"))
    sys.stdout.flush()
    time.sleep(0.05)
PY
key "python3 $SPIN"
sleep 0.3
key --key enter
sleep 0.8
grep -Fq 'kind=title raw="spin-' "$OUT/app.log" || fail "标题 spinner 没跑起来"
# 窄 Tab inactive 时 close 不显示，x=100 整块都是切换热区（52–99px 均命中本体）。
click 100 16 0.8
grep -Fq '[ws] active tab=0' "$OUT/app.log" || fail "标题 spinner 时点 Tab 没有切换（疑似点到 close）"
shot 06-spinner-tab0-active
# 关闭走键盘，避免依赖标题变化后的精确像素宽度；X 的窄 Tab 行为见手工清单。
key w --ctrl
sleep 0.8
grep -Fq '[ws] close tab=0' "$OUT/app.log" || fail "标题 spinner 时 Ctrl+W 没有关闭 Tab"
shot 07-spinner-tab0-closed

echo "PASS: TabBar +/>_ 旧交互 + 标题 spinner 下点击（shots: $OUT/shots）"
