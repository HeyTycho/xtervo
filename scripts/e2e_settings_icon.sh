#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
# Settings 齿轮 + 终端右键菜单无图标验收。
#   - 顶栏设置按钮 / Settings Tab 的图标必须是“8 齿 + 中心孔”的齿轮，
#     不能退化成太阳（圆环 + 分离光线）或搜索（圆环 + 手柄）。
#   - 终端右键菜单无图标：文字与分组标题左对齐，不留 16px 图标空槽。
#
# 在独立 headless Xvfb 上运行（默认 :138），只 kill 本脚本启动的
# Xvfb/OpenBox/app，不占用 :1、不启动 Xephyr。
#
# 用法：
#   cargo build -p tervo-app --bin tervo --bin xwin --bin xclick --bin xkey
#   ./scripts/e2e_settings_icon.sh
# 环境变量：E2E_BIN、E2E_OUT、E2E_DISPLAY、TERVO_PERF_DIR、E2E_XVFB_BIN。
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT=$PWD
OUT=${E2E_OUT:-$ROOT/.logs/settings-icon}
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

DISPLAY_NUM=${E2E_DISPLAY:-:138}
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

echo "== 启动 Xvfb $DISPLAY_NUM =="
"$XVFB_BIN" "$DISPLAY_NUM" -screen 0 1600x1000x24 -ac -nolisten tcp >"$OUT/xvfb.log" 2>&1 &
XVFB_PID=$!
for _ in $(seq 1 100); do xdpyinfo >/dev/null 2>&1 && break; sleep 0.05; done
xdpyinfo >/dev/null 2>&1 || { echo "Xvfb 启动失败：$OUT/xvfb.log" >&2; exit 2; }
if [ -x "$OPENBOX" ]; then
    LD_LIBRARY_PATH=$OPENBOX_LIB "$OPENBOX" >"$OUT/openbox.log" 2>&1 &
    OB_PID=$!
    sleep 0.6
fi

fail() {
    echo "FAIL: $*" >&2
    [ -n "${APP_PID:-}" ] && { echo "--- app log tail ---" >&2; tail -80 "$OUT/app.log" >&2 || true; }
    exit 1
}

cat > "$OUT/cfg.toml" <<CFG
theme = "tokyo-night"
font_size = 15
confirm_close = false
CFG

TERVO_CONFIG="$OUT/cfg.toml" "$BIN" --cols 120 --rows 32 --title Tervo \
    --shell /bin/sh -- -c 'printf "READY\n"; sleep 900' >"$OUT/app.log" 2>&1 &
APP_PID=$!
WIN=""
for _ in $(seq 1 200); do
    WIN=$(xwininfo -root -tree 2>/dev/null | grep -i '"tervo"' | head -1 | awk '{print $1}' || true)
    [ -n "$WIN" ] && break
    sleep 0.05
done
[ -n "$WIN" ] || fail "找不到 Tervo 窗口"
"$XWIN" Tervo 20 20 >/dev/null 2>&1 || true
sleep 1.0

read_geom() {
    WIN=$(xwininfo -root -tree 2>/dev/null | grep -i '"tervo"' | head -1 | awk '{print $1}')
    ABS_X=$(xwininfo -id "$WIN" | awk '/Absolute upper-left X/{print $NF}')
    ABS_Y=$(xwininfo -id "$WIN" | awk '/Absolute upper-left Y/{print $NF}')
    W=$(xwininfo -id "$WIN" | awk '/Width:/{print $2}')
    H=$(xwininfo -id "$WIN" | awk '/Height:/{print $2}')
    echo "  window=${W}x${H} at $ABS_X,$ABS_Y"
}
read_geom

shot() {
    xwd -silent -id "$WIN" -out "$OUT/$1.xwd"
    ffmpeg -y -hide_banner -loglevel error -f xwd_pipe -i "$OUT/$1.xwd" "$OUT/$1.png"
    rm -f "$OUT/$1.xwd"
    echo "  shot $OUT/$1.png"
}
click_content() { "$XCLICK" "$((ABS_X + $1))" "$((ABS_Y + $2))" "${3:-1}" >/dev/null 2>&1; sleep 0.30; }
move_content() { "$XCLICK" "$((ABS_X + $1))" "$((ABS_Y + $2))" >/dev/null 2>&1; sleep 0.30; }
# 只移动指针、不按键（xclick 总会点击，这里用 libXtst 的 motion 事件）。
hover_content() {
    python3 - "$((ABS_X + $1))" "$((ABS_Y + $2))" <<'PYMOVE'
import ctypes, sys
x, y = int(sys.argv[1]), int(sys.argv[2])
x11 = ctypes.CDLL('libX11.so.6')
xtst = ctypes.CDLL('libXtst.so.6')
x11.XOpenDisplay.restype = ctypes.c_void_p
x11.XOpenDisplay.argtypes = [ctypes.c_char_p]
x11.XFlush.argtypes = [ctypes.c_void_p]
x11.XCloseDisplay.argtypes = [ctypes.c_void_p]
xtst.XTestFakeMotionEvent.argtypes = [ctypes.c_void_p, ctypes.c_int, ctypes.c_int,
                                      ctypes.c_int, ctypes.c_ulong]
d = x11.XOpenDisplay(None)
if not d:
    sys.exit(1)
xtst.XTestFakeMotionEvent(d, -1, x, y, 0)
x11.XFlush(d)
x11.XCloseDisplay(d)
PYMOVE
    sleep 0.35
}
gear_click() { click_content $((W - 118)) 16; }
tab_click() { click_content 100 11; }
key() { "$XKEY" "$@" >/dev/null 2>&1; sleep 0.20; }

# 顶栏齿轮签名：中心孔（bg）+ r=4.2 整圈轮体 + r=6.2 八个齿。
check_gear() { # <png> top|tab
    python3 - "$1" "$2" "$W" <<'PY' || fail "齿轮检查失败：$1 ($2)"
from PIL import Image
import math, statistics, sys
path, mode, W = sys.argv[1], sys.argv[2], int(sys.argv[3])
im = Image.open(path).convert('RGB')
px = im.load()
iw, ih = im.size

def val(x, y):
    return max(px[int(round(x)), int(round(y))])

def score(cx, cy):
    bg = int(statistics.median([val(cx + dx, cy + dy) for dx in (-7, 7) for dy in (-7, 7)]))
    ring = sum(val(cx + 4.2 * math.cos(math.radians(a)),
                  cy + 4.2 * math.sin(math.radians(a))) > bg + 40
               for a in range(0, 360, 22))
    teeth = sum(val(cx + 6.2 * math.cos(math.radians(a)),
                    cy + 6.2 * math.sin(math.radians(a))) > bg + 40
                for a in range(0, 360, 45))
    hole = val(cx, cy) <= bg + 25
    return bg, ring, teeth, hole

if mode == 'top':
    cx, cy = W - 118, 16
    bg, ring, teeth, hole = score(cx, cy)
    print(f'  top gear at {cx},{cy}: ring={ring}/17 teeth={teeth}/8 hole={hole} bg={bg}')
    if ring < 15 or teeth < 8 or not hole:
        sys.exit(1)
elif mode == 'tab':
    hits = []
    for cy in range(2, 23):
        for cx in range(120, min(W // 2, 420)):
            bg = int(statistics.median([val(cx + dx, cy + dy) for dx in (-7, 7) for dy in (-7, 7)]))
            ring = sum(val(cx + 4.2 * math.cos(math.radians(a)),
                          cy + 4.2 * math.sin(math.radians(a))) > bg + 40
                       for a in range(0, 360, 22))
            if ring < 15:
                continue
            teeth = sum(val(cx + 6.2 * math.cos(math.radians(a)),
                            cy + 6.2 * math.sin(math.radians(a))) > bg + 40
                        for a in range(0, 360, 45))
            if teeth < 8:
                continue
            if val(cx, cy) > bg + 25:
                continue
            hits.append((cx, cy, bg, ring, teeth))
    print(f'  tab gear hits={len(hits)} {hits[:3]}')
    if not hits:
        sys.exit(1)
PY
}

# 右键菜单：面板 280 宽；行文字从面板 +10 开始；左侧 16px 图标槽不存在。
check_menu() { # <png> <wmin> <wmax> <hmin> <hmax>
    python3 - "$1" "$2" "$3" "$4" "$5" <<'PY' || fail "右键菜单检查失败：$1"
from PIL import Image
import sys
path, wmin, wmax, hmin, hmax = sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), int(sys.argv[4]), int(sys.argv[5])
im = Image.open(path).convert('RGB')
px = im.load()
w, h = im.size
raise_rgb = (36, 40, 59)  # Tokyo Night Palette.raise = #24283b
rows = []
for y in range(h):
    best = cur = 0
    for x in range(w):
        if px[x, y] == raise_rgb:
            cur += 1
            best = max(best, cur)
        else:
            cur = 0
    if best >= 150:
        rows.append(y)
if not rows:
    print('  menu panel not found'); sys.exit(1)
y0, y1 = min(rows), max(rows)
cols = [x for x in range(w)
        if sum(1 for y in range(y0, y1 + 1) if px[x, y] == raise_rgb) >= 40]
if not cols:
    print('  menu panel columns not found'); sys.exit(1)
x0, x1 = min(cols), max(cols)
bw, bh = x1 - x0 + 1, y1 - y0 + 1
print(f'  menu bbox x={x0} y={y0} {bw}x{bh}')
if not (wmin <= bw <= wmax):
    print(f'  menu width {bw} not in [{wmin},{wmax}]'); sys.exit(1)
if not (hmin <= bh <= hmax):
    print(f'  menu height {bh} not in [{hmin},{hmax}]'); sys.exit(1)

# 行文字（亮色 #c0caf5）按 y 聚成 band；分组标题是 subtext，亮度<150 不参与。
raw_bands = []
cur = []
for y in range(y0 + 4, y1 - 3):
    n = sum(1 for x in range(x0 + 4, x1 - 4) if max(px[x, y]) > 150)
    if n >= 2:
        if cur and y > cur[-1] + 3:
            raw_bands.append((cur[0], cur[-1])); cur = []
        cur.append(y)
    elif cur:
        raw_bands.append((cur[0], cur[-1])); cur = []
if cur:
    raw_bands.append((cur[0], cur[-1]))
bands = [b for b in raw_bands if b[1] - b[0] >= 2]
if len(bands) < 10:
    print(f'  only {len(bands)} text bands'); sys.exit(1)
worst_left = 0
for (by0, by1) in bands:
    xs = [x for y in range(by0, by1 + 1) for x in range(x0 + 2, x1 - 2) if max(px[x, y]) > 150]
    left = min(xs) - x0
    worst_left = max(worst_left, left)
    if left > 20:
        print(f'  row y={by0} text starts at +{left}, icon slot still reserved'); sys.exit(1)
    # 左侧 6..27px 必须有亮色字形像素；旧图标槽里是 subtext 图标，没有亮色。
    bright = sum(1 for y in range(by0, by1 + 1) for x in range(x0 + 6, x0 + 28)
                 if max(px[x, y]) > 180)
    if bright < 2:
        print(f'  row y={by0} has no label glyph in former icon slot'); sys.exit(1)
print(f'  {len(bands)} label rows, max left offset={worst_left}px (no icon gutter)')
PY
}

echo "== 1) 初始终端：右上角设置按钮 =="
shot 01_terminal_closed
check_gear "$OUT/01_terminal_closed.png" top

echo "== 2) 打开 Settings：Tab 齿轮 + 按钮 active =="
gear_click
shot 02_settings_active
check_gear "$OUT/02_settings_active.png" tab
check_gear "$OUT/02_settings_active.png" top

echo "== 3) 回到终端：Settings Tab inactive（Tab 保留）=="
tab_click
shot 03_tab_inactive
check_gear "$OUT/03_tab_inactive.png" tab
check_gear "$OUT/03_tab_inactive.png" top

echo "== 5) 终端右键菜单：无图标、文字左对齐 =="
click_content 300 200 3
shot 05_context_menu
check_menu "$OUT/05_context_menu.png" 276 284 470 502
hover_content 430 243
shot 06_context_menu_hover
hover_content 430 385
shot 07_context_menu_danger_hover
key --key escape

echo "== 6) 窄窗口 700x500 =="
"$XWIN" Tervo 20 20 700 500 >/dev/null 2>&1 || true
sleep 0.9
read_geom
shot 08_narrow_terminal
check_gear "$OUT/08_narrow_terminal.png" top
gear_click
shot 09_narrow_settings
check_gear "$OUT/09_narrow_settings.png" tab
check_gear "$OUT/09_narrow_settings.png" top
key --key escape
click_content 250 200 3
shot 10_narrow_context_menu
check_menu "$OUT/10_narrow_context_menu.png" 276 284 280 502
key --key escape

echo "== 7) 宽窗口 1400x900 =="
"$XWIN" Tervo 20 20 1400 900 >/dev/null 2>&1 || true
sleep 0.9
read_geom
gear_click
shot 11_wide_settings
check_gear "$OUT/11_wide_settings.png" tab
check_gear "$OUT/11_wide_settings.png" top

echo "PASS: gear + context menu screenshots in $OUT"
