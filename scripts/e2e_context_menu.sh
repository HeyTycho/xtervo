#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
# P1-10 终端右键菜单美化 e2e：分组/字号/链接菜单/窄窗滚动/右下翻转/亮暗主题。
#
# 在独立 headless Xvfb 上运行（默认 :135），只 kill 本脚本启动的 Xvfb/OpenBox/app，
# 不占用 :1、不启动 Xephyr、不 pkill 其他进程。
#
# 用法：
#   cargo build -p tervo-app --bin tervo --bin xwin --bin xclick --bin xkey
#   ./scripts/e2e_context_menu.sh
#
# 环境变量：E2E_BIN、E2E_OUT、E2E_DISPLAY、TERVO_PERF_DIR、E2E_XVFB_BIN。
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT=$PWD
OUT=${E2E_OUT:-$ROOT/.logs/context-menu}
mkdir -p "$OUT" "$OUT/home" "$OUT/logs" /tmp/.X11-unix

BIN=${E2E_BIN:-$ROOT/target/debug/tervo}
XWIN=$ROOT/target/debug/xwin
XCLICK=$ROOT/target/debug/xclick
XKEY=$ROOT/target/debug/xkey
for f in "$BIN" "$XWIN" "$XCLICK" "$XKEY"; do
    [ -x "$f" ] || { echo "缺少 $f；先 cargo build -p tervo-app --bin tervo --bin xwin --bin xclick --bin xkey" >&2; exit 2; }
done

COMMON=$(git rev-parse --git-common-dir)
MAIN_ROOT=$(cd "$(dirname "$COMMON")" && pwd)
PERF=${TERVO_PERF_DIR:-$MAIN_ROOT/.perf}
XVFB_BIN=${E2E_XVFB_BIN:-$PERF/xvfb/usr/bin/Xvfb}
[ -x "$XVFB_BIN" ] || XVFB_BIN=$(command -v Xvfb || true)
[ -x "$XVFB_BIN" ] || { echo "找不到 Xvfb；不会回退到 Xephyr" >&2; exit 2; }
OPENBOX=$PERF/openbox/usr/bin/openbox
OPENBOX_LIB=$PERF/openbox/usr/lib/x86_64-linux-gnu

DISPLAY_NUM=${E2E_DISPLAY:-:135}
export DISPLAY=$DISPLAY_NUM
export HOME=$OUT/home
export XMODIFIERS=@im=none
export SLINT_BACKEND=winit-software
export TERVO_LOG_DIR=$OUT/logs

if xdpyinfo >/dev/null 2>&1; then
    echo "display $DISPLAY_NUM 已被占用；本脚本退出以避免干扰。" >&2
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

cat > "$OUT/shell.sh" <<SHELL
#!/bin/sh
printf 'http://example.com/ctx-menu-demo\\nAlpha alpha\\nabc123\\nREADY\\n'
while IFS= read -r line; do
  printf 'OUT:%s\\n' "\$line"
done
SHELL
chmod +x "$OUT/shell.sh"

echo "== 启动 Xvfb $DISPLAY_NUM =="
"$XVFB_BIN" "$DISPLAY_NUM" -screen 0 1280x800x24 -ac -nolisten tcp >"$OUT/xvfb.log" 2>&1 &
XVFB_PID=$!
for _ in $(seq 1 100); do xdpyinfo >/dev/null 2>&1 && break; sleep 0.05; done
xdpyinfo >/dev/null 2>&1 || { echo "Xvfb 启动失败：$OUT/xvfb.log" >&2; exit 2; }

if [ -x "$OPENBOX" ]; then
    LD_LIBRARY_PATH=$OPENBOX_LIB "$OPENBOX" >"$OUT/openbox.log" 2>&1 &
    OB_PID=$!
    sleep 0.6
else
    echo "提示：未找到仓库 .perf/openbox，继续无 WM 运行" >&2
fi

write_config() { # <theme> <path>
    cat > "$2" <<CFG
theme = "$1"
font_size = 15
confirm_close = false
desktop_notifications = false
confirm_multiline_paste = false
copy_on_select = true
CFG
}

APP_PID=""
WIN=""
ABS_X=0
ABS_Y=0
W=0
H=0
LOG=""

read_geometry() {
    read -r ABS_X ABS_Y W H < <(xwininfo -id "$WIN" | awk '
        /Absolute upper-left X/{ax=$NF}
        /Absolute upper-left Y/{ay=$NF}
        /Width:/{w=$2}
        /Height:/{h=$2}
        END{print ax, ay, w, h}')
}

stop_app() {
    if [ -n "$APP_PID" ]; then
        kill "$APP_PID" 2>/dev/null || true
        wait "$APP_PID" 2>/dev/null || true
        APP_PID=""
        sleep 0.3
    fi
    WIN=""
}

start_app() { # <tag> <theme> <cols> <rows> <x> <y> [w h]
    local tag=$1 theme=$2 cols=$3 rows=$4 x=$5 y=$6 w=${7:-} h=${8:-}
    stop_app
    write_config "$theme" "$OUT/$tag.toml"
    LOG=$OUT/$tag.log
    TERVO_CONFIG="$OUT/$tag.toml" \
        "$BIN" --cols "$cols" --rows "$rows" --title Tervo --shell /bin/sh -- "$OUT/shell.sh" >"$LOG" 2>&1 &
    APP_PID=$!
    WIN=""
    for _ in $(seq 1 200); do
        WIN=$(xwininfo -root -tree 2>/dev/null | grep -i '"tervo"' | head -1 | awk '{print $1}' || true)
        [ -n "$WIN" ] && break
        sleep 0.05
    done
    [ -n "$WIN" ] || { echo "找不到 Tervo 窗口（$tag）；见 $LOG" >&2; tail -40 "$LOG" >&2; exit 2; }
    if [ -n "$w" ]; then
        "$XWIN" Tervo "$x" "$y" "$w" "$h" >/dev/null 2>&1 || true
    else
        "$XWIN" Tervo "$x" "$y" >/dev/null 2>&1 || true
    fi
    sleep 0.9
    WIN=$(xwininfo -root -tree 2>/dev/null | grep -i '"tervo"' | head -1 | awk '{print $1}')
    read_geometry
    echo "  [$tag] window=$WIN at $ABS_X,$ABS_Y ${W}x${H} log=$LOG"
}

resize_window() { # <w> <h>
    "$XWIN" Tervo "$ABS_X" "$ABS_Y" "$1" "$2" >/dev/null 2>&1 || true
    sleep 1.0
    WIN=$(xwininfo -root -tree 2>/dev/null | grep -i '"tervo"' | head -1 | awk '{print $1}')
    read_geometry
    echo "  resize -> ${W}x${H}"
}

shot() {
    xwd -silent -id "$WIN" -out "$OUT/$1.xwd"
    ffmpeg -y -hide_banner -loglevel error -f xwd_pipe -i "$OUT/$1.xwd" "$OUT/$1.png"
    rm -f "$OUT/$1.xwd"
    [ -s "$OUT/$1.png" ] || fail "截图失败：$1"
}

click_abs() { "$XCLICK" "$1" "$2" "${3:-1}" >/dev/null 2>&1; sleep 0.24; }
click_content() { click_abs $((ABS_X + $1)) $((ABS_Y + 32 + $2)) "${3:-1}"; }
key() { "$XKEY" "$@" >/dev/null 2>&1; sleep 0.18; }
wheel() { "$XCLICK" "$1" "$2" "$3" >/dev/null 2>&1; sleep 0.25; }

count_log() { grep -Fc -- "$1" "$LOG" 2>/dev/null || true; }
expect_log() { grep -Fq -- "$1" "$LOG" || fail "日志缺少: $1"; }
fail() {
    echo "FAIL: $*" >&2
    echo "--- app log tail ---" >&2
    tail -120 "$LOG" >&2
    exit 1
}

# 点菜单外关闭（左键；不会像右键那样重新弹菜单），并确认 Rust 收到 dismissed。
close_menu() {
    local before
    before=$(count_log '[ui] menu dismissed')
    click_content 5 5 1
    for _ in $(seq 1 40); do
        [ "$(count_log '[ui] menu dismissed')" -gt "$before" ] && return 0
        sleep 0.05
    done
    key --key escape
    for _ in $(seq 1 40); do
        [ "$(count_log '[ui] menu dismissed')" -gt "$before" ] && return 0
        sleep 0.05
    done
    fail "菜单未能关闭"
}

# 菜单面板 bbox：扫描每行最长的 raise 色连续段，超过 100px 的行视为菜单。
menu_bbox() { # <png> <raise-hex>
    python3 - "$1" "$2" <<'PY'
from PIL import Image
import sys
im = Image.open(sys.argv[1]).convert('RGB')
w, h = im.size
hexs = sys.argv[2].lstrip('#')
c = tuple(int(hexs[i:i+2], 16) for i in (0, 2, 4))
rows = []
for y in range(h):
    best = cur = 0
    for x in range(w):
        if im.getpixel((x, y)) == c:
            cur += 1
            best = max(best, cur)
        else:
            cur = 0
    if best >= 100:
        rows.append(y)
if not rows:
    print('NONE')
    sys.exit(0)
y0, y1 = min(rows), max(rows)
cols = []
for x in range(w):
    cnt = 0
    for y in range(y0, y1 + 1):
        if im.getpixel((x, y)) == c:
            cnt += 1
    if cnt >= 100:
        cols.append(x)
if not cols:
    print('NONE')
    sys.exit(0)
x0, x1 = min(cols), max(cols)
print(x0, y0, x1 - x0 + 1, y1 - y0 + 1)
PY
}

# assert_bbox <png> <raise-hex> <wmin> <wmax> <hmin> <hmax> <max-x> <max-y>
assert_bbox() {
    local png=$1 color=$2 wmin=$3 wmax=$4 hmin=$5 hmax=$6 maxx=$7 maxy=$8 bbox bx by bw bh
    bbox=$(menu_bbox "$png" "$color")
    [ "$bbox" != "NONE" ] || fail "截图里找不到菜单面板：$png"
    read -r bx by bw bh <<<"$bbox"
    [ "$bw" -ge "$wmin" ] && [ "$bw" -le "$wmax" ] \
        || fail "$png 菜单宽度 $bw 不在 [$wmin,$wmax]（bbox=$bbox）"
    [ "$bh" -ge "$hmin" ] && [ "$bh" -le "$hmax" ] \
        || fail "$png 菜单高度 $bh 不在 [$hmin,$hmax]（bbox=$bbox）"
    [ $((bx + bw)) -le "$maxx" ] || fail "$png 菜单右侧溢出：bbox=$bbox maxx=$maxx"
    [ $((by + bh)) -le "$maxy" ] || fail "$png 菜单底部溢出：bbox=$bbox maxy=$maxy"
    echo "  bbox=$bbox (x=$bx y=$by ${bw}x${bh})"
}

diff_pixels() { # <a.png> <b.png> <x0> <y0> <x1> <y1> <min-diff>
    python3 - "$1" "$2" "$3" "$4" "$5" "$6" "$7" <<'PY'
from PIL import Image
import sys
a = Image.open(sys.argv[1]).convert('RGB')
b = Image.open(sys.argv[2]).convert('RGB')
x0, y0, x1, y1 = map(int, sys.argv[3:7])
limit = int(sys.argv[7])
pa, pb = a.load(), b.load()
n = sum(1 for y in range(y0, min(y1, a.size[1])) for x in range(x0, min(x1, a.size[0]))
        if pa[x, y] != pb[x, y])
print(n)
sys.exit(0 if n >= limit else 1)
PY
}

RAISE_DARK=#24283b
RAISE_LIGHT=#e5ddc7

# ================================================================ 暗色：普通/选区/链接
echo "== 1) 普通菜单（暗色 Tokyo Night，96x32） =="
start_app dark tokyo-night 96 32 10 10
# 第一次左键先把窗口焦点坐实，避免 Xvfb 首击只用于 focus。
click_content 300 300 1
click_content 300 12 3
sleep 0.35
expect_log '[ui] right click pane=1 at 300,12 link=false'
shot menu_normal
assert_bbox "$OUT/menu_normal.png" "$RAISE_DARK" 270 284 470 500 $W $H
# 菜单锚点：x=300、y=窗口 44，不翻转。
python3 - "$OUT/menu_normal.png" "$RAISE_DARK" <<'PY' || fail "普通菜单锚点不对（应贴右键处）"
from PIL import Image
import sys
im = Image.open(sys.argv[1]).convert('RGB')
c = tuple(int(sys.argv[2].lstrip('#')[i:i+2], 16) for i in (0, 2, 4))
# 在 x=310 附近找第一行/列面板色：应≈(300,44)
ys = [y for y in range(im.size[1]) if im.getpixel((320, y)) == c]
xs = [x for x in range(im.size[0]) if im.getpixel((x, 60)) == c]
assert ys and xs, 'panel not found'
assert 40 <= min(ys) <= 48, f'menu top {min(ys)} != 44'
assert 297 <= min(xs) <= 305, f'menu left {min(xs)} != ~300'
PY
# 菜单外左键关闭（键盘 Esc 路径本轮不改；现有 IME/焦点逻辑保持原样）。
close_menu

echo "== 2) 有选区时菜单（选区高亮仍在，Copy 可用） =="
click_content 300 300 1
shot select_before
"$XCLICK" $((ABS_X + 10)) $((ABS_Y + 32 + 33)) drag $((ABS_X + 130)) $((ABS_Y + 32 + 33)) >/dev/null 2>&1
sleep 0.45
shot select_after
diff_pixels "$OUT/select_before.png" "$OUT/select_after.png" 10 54 140 76 200 \
    || fail "拖拽后没有看到选区高亮"
click_content 400 200 3
sleep 0.35
expect_log '[ui] right click pane=1 at 400,200 link=false'
shot menu_selection
assert_bbox "$OUT/menu_selection.png" "$RAISE_DARK" 270 284 470 500 $W $H
close_menu

echo "== 3) 链接菜单（Open Link / Copy Link 在最上组） =="
click_content 40 11 3
sleep 0.35
expect_log '[ui] right click pane=1 at 40,11 link=true'
shot menu_link
assert_bbox "$OUT/menu_link.png" "$RAISE_DARK" 270 284 550 580 $W $H
# Copy Link 仍走 action id 10：点击第 2 行，验证剪贴板拿到 URL。
# 菜单：anchor(40,11) + 6 padding + Link 组标题 22 + Open Link 30 + 半行 15 = 84。
click_abs $((ABS_X + 40 + 140)) $((ABS_Y + 32 + 84)) 1
sleep 0.35
expect_log '[link] copy uri='
CLIP=$(xsel -b -o 2>/dev/null || true)
[ "$CLIP" = "http://example.com/ctx-menu-demo" ] \
    || fail "Copy Link 剪贴板内容不对：$CLIP"
echo "  clipboard=$CLIP"

# ================================================================ 窄窗 480x360
echo "== 4) 窄窗 480x360：菜单不溢出 + 可滚动 =="
resize_window 480 360
click_content 240 150 1
click_content 240 150 3
sleep 0.35
expect_log '[ui] right click pane=1 at 240,150 link=false'
shot menu_narrow_480x360
# 280x316 的菜单应夹在 480x360 内容区内，四周留边。
assert_bbox "$OUT/menu_narrow_480x360.png" "$RAISE_DARK" 270 284 300 325 477 358
# 视口 < 内容高度 -> 滚轮滚动应改变菜单可视区域。
wheel $((ABS_X + 320)) $((ABS_Y + 32 + 200)) 5
wheel $((ABS_X + 320)) $((ABS_Y + 32 + 200)) 5
wheel $((ABS_X + 320)) $((ABS_Y + 32 + 200)) 5
sleep 0.3
shot menu_narrow_scrolled
diff_pixels "$OUT/menu_narrow_480x360.png" "$OUT/menu_narrow_scrolled.png" 190 38 475 360 300 \
    || fail "窄窗菜单滚轮没有滚动内容"
close_menu

# ================================================================ 靠右下翻转（864x700）
echo "== 5) 靠右下右键：菜单向左上翻转且不溢出 =="
resize_window 864 700
click_content 400 300 1
click_content 804 608 3
sleep 0.35
expect_log '[ui] right click pane=1 at 804,608 link=false'
shot menu_bottom_right
# 菜单 280x490：x 翻到 524、y 翻到 118，全部留在窗口内。
assert_bbox "$OUT/menu_bottom_right.png" "$RAISE_DARK" 270 284 470 500 $W $H
python3 - "$OUT/menu_bottom_right.png" "$RAISE_DARK" <<'PY' || fail "靠右下菜单没有向左上翻转"
from PIL import Image
import sys
im = Image.open(sys.argv[1]).convert('RGB')
c = tuple(int(sys.argv[2].lstrip('#')[i:i+2], 16) for i in (0, 2, 4))
ys = [y for y in range(im.size[1]) if im.getpixel((540, y)) == c]
assert ys and max(ys) < 700 - 60, f'menu 未向上翻转: max_y={max(ys) if ys else None}'
PY
close_menu

# ================================================================ 亮色主题
echo "== 6) 亮色主题（Solarized Light）：菜单文字/分隔仍清楚 =="
start_app light solarized-light 96 32 10 10
click_content 300 300 1
click_content 300 12 3
sleep 0.35
expect_log '[ui] right click pane=1 at 300,12 link=false'
shot menu_light
assert_bbox "$OUT/menu_light.png" "$RAISE_LIGHT" 270 284 470 500 $W $H
close_menu

echo "OK: $OUT"
echo "截图：menu_normal.png menu_selection.png menu_link.png menu_narrow_480x360.png menu_narrow_scrolled.png menu_bottom_right.png menu_light.png"
