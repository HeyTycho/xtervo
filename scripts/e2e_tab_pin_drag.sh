#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
# Tab 级 Pin + 拖拽 ghost/插入线/让位动画 e2e（headless Xvfb :158）。
#
# 覆盖：
#   1. 右键 Pin Tab → 移到 pinned 区末尾；再 Pin 第二个 → 追加在最后一个 pinned 后；
#   2. Unpin Tab → 回到第一个 unpinned 前；截图可见 pin 角标与分组顺序；
#   3. 正常关闭写 session.json（pinned 字段 additive），重启恢复 pinned 顺序；
#   4. 拖拽中间态截图：半透明 ghost + 2px accent 插入线；松手后排序；
#   5. pinned 拖到 unpinned / 反向拖动：不亮线、不移动；
#   6. 拖拽中按 Esc：取消并复位，松手不排序。
#
# 用法：
#   cargo build -p tervo-app --bin tervo --bin xwin --bin xclick --bin xkey
#   ./scripts/e2e_tab_pin_drag.sh
# 环境变量：TERVO_BIN / XWIN_BIN / XCLICK_BIN / XKEY_BIN / TAB_PIN_DRAG_OUT / E2E_DISPLAY。
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT=$PWD
OUT=${TAB_PIN_DRAG_OUT:-$ROOT/.logs/tab-pin-drag/e2e}
mkdir -p "$OUT/shots" "$OUT/home" "$OUT/logs" /tmp/.X11-unix
rm -f "$OUT"/shots/*.png "$OUT"/app_*.log "$OUT/session.json" "$OUT/session.json.bak"

COMMON=$(git rev-parse --git-common-dir)
MAIN_ROOT=$(cd "$(dirname "$COMMON")" && pwd)
PERF=${TERVO_PERF_DIR:-$MAIN_ROOT/.perf}
XVFB_BIN=${E2E_XVFB_BIN:-$PERF/xvfb/usr/bin/Xvfb}
[ -x "$XVFB_BIN" ] || XVFB_BIN=$(command -v Xvfb || true)
[ -x "$XVFB_BIN" ] || { echo "找不到 Xvfb；不会回退到 Xephyr" >&2; exit 2; }
OPENBOX=$PERF/openbox/usr/bin/openbox
OPENBOX_LIB=$PERF/openbox/usr/lib/x86_64-linux-gnu

BIN=${TERVO_BIN:-$MAIN_ROOT/target/debug/tervo}
XWIN=${XWIN_BIN:-$MAIN_ROOT/target/debug/xwin}
XCLICK=${XCLICK_BIN:-$MAIN_ROOT/target/debug/xclick}
XKEY=${XKEY_BIN:-$MAIN_ROOT/target/debug/xkey}
for f in "$BIN" "$XWIN" "$XCLICK" "$XKEY"; do
    [ -x "$f" ] || { echo "缺少 $f（先 cargo build -p tervo-app --bin tervo --bin xwin --bin xclick --bin xkey）" >&2; exit 2; }
done

export DISPLAY=${E2E_DISPLAY:-:158}
export HOME=$OUT/home
export LANG=C.UTF-8
export LC_ALL=C.UTF-8
export XMODIFIERS=@im=none
export SLINT_BACKEND=winit-software
if xdpyinfo >/dev/null 2>&1; then
    echo "display $DISPLAY 已被占用；本脚本退出以避免干扰。" >&2
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

WIN=""
ABS_X=0
ABS_Y=0
W=0
H=0

refresh_geom() {
    WIN=$(xwininfo -root -tree 2>/dev/null | grep -i '"tervo"' | head -1 | awk '{print $1}')
    [ -n "$WIN" ] || { echo "找不到 Tervo 窗口" >&2; exit 2; }
    ABS_X=$(xwininfo -id "$WIN" | awk '/Absolute upper-left X/{print $NF}')
    ABS_Y=$(xwininfo -id "$WIN" | awk '/Absolute upper-left Y/{print $NF}')
    W=$(xwininfo -id "$WIN" | awk '/Width:/{print $2}')
    H=$(xwininfo -id "$WIN" | awk '/Height:/{print $2}')
}

start_app() {  # <tag>
    local tag=$1
    local log="$OUT/app_$tag.log"
    : >"$log"
    TERVO_CONFIG="$OUT/config.toml" \
    TERVO_SESSION="$OUT/session.json" \
    TERVO_LOG_DIR="$OUT/logs" \
        "$BIN" --cols 120 --rows 32 --title Tervo --shell /bin/sh >"$log" 2>&1 &
    APP_PID=$!
    WIN=""
    for _ in $(seq 1 240); do
        WIN=$(xwininfo -root -tree 2>/dev/null | grep -i '"tervo"' | head -1 | awk '{print $1}' || true)
        [ -n "$WIN" ] && break
        sleep 0.05
    done
    [ -n "$WIN" ] || { echo "启动失败（$tag）：$log" >&2; exit 2; }
    "$XWIN" Tervo 20 20 >/dev/null 2>&1 || true
    sleep 1.0
    refresh_geom
    if [ "$W" != 1080 ] || [ "$H" != 736 ]; then
        echo "窗口 ${W}x${H} 与坐标假设 1080x736（cell 9x22 / 120x32）不符" >&2
        exit 2
    fi
    tab_y=$((ABS_Y + 16))
    menu_pin_y=$((ABS_Y + 230))
    echo "  app=$tag window=${W}x${H} at ${ABS_X},${ABS_Y} log=$log"
}

stop_app_graceful() {
    [ -n "$APP_PID" ] || return 0
    # 右上角 ✕：正常退出 → save_session_on_exit 写快照。
    "$XCLICK" "$((ABS_X + W - 16))" "$((ABS_Y + 16))" >/dev/null 2>&1 || true
    for _ in $(seq 1 160); do
        kill -0 "$APP_PID" 2>/dev/null || break
        sleep 0.05
    done
    wait "$APP_PID" 2>/dev/null || true
    APP_PID=""
    sleep 0.5
}

shot() {
    local name=$1
    refresh_geom
    xwd -silent -id "$WIN" -out "$OUT/shots/$name.xwd"
    python3 "$ROOT/scripts/xwd2png.py" "$OUT/shots/$name.xwd" "$OUT/shots/$name.png" >/dev/null
    rm -f "$OUT/shots/$name.xwd"
    echo "  shot $OUT/shots/$name.png"
}

click_abs() { "$XCLICK" "$1" "$2" "${3:-1}" >/dev/null 2>&1; sleep 0.3; }
focus_term() { "$XWIN" Tervo >/dev/null 2>&1 || true; sleep 0.12; }
type_text() { focus_term; "$XKEY" "$1" >/dev/null; }
set_title() {  # <T1>
    focus_term
    "$XKEY" "printf '\\033]0;$1\\007'" >/dev/null
    "$XKEY" --key enter >/dev/null
    sleep 0.5
}

# 窗口内坐标（1080x736，4 个 52px Tab，间距 4，TabBar 左侧 58px）
# slot i 靠左侧（避开 hover 才出现的 close ×）：x = 58 + i*56 + 14
# tab_y / menu_pin_y 在 start_app 里按实际窗口位置计算。
tab_x() { echo $((ABS_X + 58 + $1 * 56 + 14)); }
# 菜单从 tabctx-x 起，点击菜单内部避开最左边框；纵向第 7 行中心。
menu_x() { echo $((ABS_X + 58 + $1 * 56 + 74)); }

open_tab_menu() { click_abs "$(tab_x "$1")" "$tab_y" 3; }
pin_tab() { open_tab_menu "$1"; click_abs "$(menu_x "$1")" "$menu_pin_y"; }

cat >"$OUT/config.toml" <<'EOF'
version = 1
theme = "tokyo-night"
font_size = 15
scrollback = 2000
confirm_close = false
confirm_multiline_paste = false
desktop_notifications = false
restore_session_on_startup = true
EOF

echo "== 1. 启动并构造 4 个有标题的 Tab =="
start_app A
set_title T1
for t in T2 T3 T4; do
    "$XKEY" t --ctrl >/dev/null
    sleep 0.9
    set_title "$t"
done
grep -q 'kind=title' "$OUT/app_A.log" || { echo "OSC 标题未生效" >&2; exit 2; }
# 第一次左键坐实焦点，避免 Xvfb 首击只用于 focus。
click_abs $((ABS_X + 300)) $((ABS_Y + 32 + 100)) 1
shot 01-four-tabs

echo "== 2. Pin T2、Pin T3：pinned 顺序追加；Unpin T2 回第一个 unpinned =="
pin_tab 1
grep -q '\[ws\] pin tab 1' "$OUT/app_A.log" || { echo "Pin T2 未生效" >&2; tail -80 "$OUT/app_A.log" >&2; exit 2; }
shot 02-pin-t2
pin_tab 2
grep -q '\[ws\] pin tab 2' "$OUT/app_A.log" || { echo "Pin T3 未生效" >&2; exit 2; }
shot 03-pin-t3
# 现在顺序 T2,T3,T1,T4；Unpin 第一个 pinned T2 → 第一个 unpinned（T3 后）
open_tab_menu 0
click_abs "$(menu_x 0)" "$menu_pin_y"
grep -q '\[ws\] unpin tab 0' "$OUT/app_A.log" || { echo "Unpin T2 未生效" >&2; exit 2; }
shot 04-unpin-t2

echo "== 3. Pin T4 后正常关闭，检查快照 pinned；重启恢复 =="
# 当前 T3(pin),T2,T1,T4；T4 在第 4 槽
pin_tab 3
grep -q '\[ws\] pin tab 3' "$OUT/app_A.log" || { echo "Pin T4 未生效" >&2; exit 2; }
shot 05-before-restart
stop_app_graceful
[ -s "$OUT/session.json" ] || { echo "退出未写 session.json" >&2; exit 2; }
python3 - "$OUT/session.json" <<'PY'
import json, sys
snap = json.load(open(sys.argv[1]))
# session v2: 顶层 windows[]；保留 v1 顶层 tabs 兼容。
rows = snap["windows"][0]["tabs"] if snap.get("windows") else snap["tabs"]
tabs = [(t["title"], bool(t.get("pinned", False))) for t in rows]
assert tabs == [("T3", True), ("T4", True), ("T2", False), ("T1", False)], tabs
print("  session pinned/order ok:", tabs)
PY

start_app B
grep -q '\[session\] restore tab "T3" pinned=true index=0' "$OUT/app_B.log" || { echo "重启未恢复 T3 pinned=0" >&2; tail -80 "$OUT/app_B.log" >&2; exit 2; }
grep -q '\[session\] restore tab "T4" pinned=true index=1' "$OUT/app_B.log" || { echo "重启未恢复 T4 pinned=1" >&2; exit 2; }
shot 06-restored

echo "== 4. 拖拽 T1(第4槽) → T2(第3槽)：ghost + 插入线 + 平滑让位 =="
BEFORE=$(grep -c '\[ws\] reorder tab' "$OUT/app_B.log" || true)
XCLICK_HOLD_MS=1400 "$XCLICK" "$(tab_x 3)" "$tab_y" drag "$(tab_x 2)" "$tab_y" >/dev/null 2>&1 &
DRAG_PID=$!
sleep 0.55
shot 07-drag-ghost-line
wait "$DRAG_PID" || true
sleep 0.7
shot 08-drag-reordered
grep -q 'tab drag from=3 .*target=2' "$OUT/app_B.log" || { echo "拖拽未实时算出 target=2" >&2; tail -120 "$OUT/app_B.log" >&2; exit 2; }
grep -q '\[ws\] reorder tab 3 → 2' "$OUT/app_B.log" || { echo "拖拽松手未排序" >&2; exit 2; }
AFTER=$(grep -c '\[ws\] reorder tab' "$OUT/app_B.log" || true)
[ "$AFTER" -eq $((BEFORE + 1)) ] || { echo "合法拖拽 reorder 次数异常 $BEFORE→$AFTER" >&2; exit 2; }

# 视觉断言：拖拽中间态相对静止态有明显差异；TabBar 内 accent 2px 插入线像素增加。
python3 - "$OUT/shots/06-restored.png" "$OUT/shots/07-drag-ghost-line.png" <<'PY'
from PIL import Image, ImageChops
import sys
a = Image.open(sys.argv[1]).convert("RGB")
b = Image.open(sys.argv[2]).convert("RGB")
diff = ImageChops.difference(a, b)
changed = sum(1 for p in diff.getdata() if max(p) > 24)
assert changed > 300, f"ghost/插入线未造成可见变化: {changed}"
# 插入线颜色 #7aa2f7（Tokyo Night accent），只数 TabBar 上半区（排除底部 active 线）。
def accent_count(im):
    n = 0
    for y in range(3, 23):
        for x in range(0, im.width):
            r, g, bl = im.getpixel((x, y))
            if abs(r - 0x7a) <= 4 and abs(g - 0xa2) <= 4 and abs(bl - 0xf7) <= 4:
                n += 1
    return n
ca, cb = accent_count(a), accent_count(b)
assert cb >= ca + 16, f"未见 2px accent 插入线: {ca} -> {cb}"
print(f"  visual ok: diff_pixels={changed} accent={ca}->{cb}")
PY

echo "== 5. pinned 跨组：T3(pinned) 拖到 T2(unpinned) 不移动/不亮线 =="
BEFORE=$(grep -c '\[ws\] reorder tab' "$OUT/app_B.log" || true)
XCLICK_HOLD_MS=900 "$XCLICK" "$(tab_x 0)" "$tab_y" drag "$(tab_x 3)" "$tab_y" >/dev/null 2>&1 &
DRAG_PID=$!
sleep 0.45
shot 09-pinned-cross-blocked
wait "$DRAG_PID" || true
sleep 0.5
grep -q 'tab drag from=0 .*target=blocked' "$OUT/app_B.log" || { echo "pinned 跨组未判 blocked" >&2; exit 2; }
AFTER=$(grep -c '\[ws\] reorder tab' "$OUT/app_B.log" || true)
[ "$AFTER" -eq "$BEFORE" ] || { echo "pinned 跨组被错误排序" >&2; exit 2; }

echo "== 6. unpinned 拖进 pinned 区：同样 blocked，不移动 =="
BEFORE=$(grep -c '\[ws\] reorder tab' "$OUT/app_B.log" || true)
XCLICK_HOLD_MS=900 "$XCLICK" "$(tab_x 3)" "$tab_y" drag "$(tab_x 0)" "$tab_y" >/dev/null 2>&1 &
DRAG_PID=$!
sleep 0.45
shot 10-unpinned-cross-blocked
wait "$DRAG_PID" || true
sleep 0.5
grep -q 'tab drag from=3 .*target=blocked' "$OUT/app_B.log" || { echo "unpinned 跨组未判 blocked" >&2; exit 2; }
AFTER=$(grep -c '\[ws\] reorder tab' "$OUT/app_B.log" || true)
[ "$AFTER" -eq "$BEFORE" ] || { echo "unpinned 跨组被错误排序" >&2; exit 2; }

echo "== 7. 拖拽中 Esc：取消并复位，松手不排序 =="
BEFORE=$(grep -c '\[ws\] reorder tab' "$OUT/app_B.log" || true)
XCLICK_HOLD_MS=1800 "$XCLICK" "$(tab_x 3)" "$tab_y" drag "$(tab_x 2)" "$tab_y" >/dev/null 2>&1 &
DRAG_PID=$!
sleep 0.55
"$XKEY" --key escape >/dev/null
sleep 0.25
shot 11-esc-cancelled
wait "$DRAG_PID" || true
sleep 0.4
grep -q '\[ui\] tab drag cancelled' "$OUT/app_B.log" || { echo "Esc 未取消拖拽" >&2; exit 2; }
AFTER=$(grep -c '\[ws\] reorder tab' "$OUT/app_B.log" || true)
[ "$AFTER" -eq "$BEFORE" ] || { echo "Esc 取消后仍排序" >&2; exit 2; }
shot 12-final

echo "== 8. 最终快照：跨组/取消不改变 pinned 顺序 =="
stop_app_graceful
python3 - "$OUT/session.json" <<'PY'
import json, sys
snap = json.load(open(sys.argv[1]))
rows = snap["windows"][0]["tabs"] if snap.get("windows") else snap["tabs"]
tabs = [(t["title"], bool(t.get("pinned", False))) for t in rows]
# 合法拖拽后：T3,T4(pinned) + T1,T2；两次跨组与 Esc 均不应改变它。
assert tabs == [("T3", True), ("T4", True), ("T1", False), ("T2", False)], tabs
print("  final session ok:", tabs)
PY

echo "PASS tab-pin-drag e2e; shots=$OUT/shots"
