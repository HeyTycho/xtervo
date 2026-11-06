#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
# 终端 UI 基础能力 e2e：可见滚动条 / 视觉铃 / 文件拖放。
#
# 在独立 headless Xvfb 上运行（默认 :115），只 kill 本脚本启动的 Xvfb/OpenBox/app，
# 不占用 :1、不启动 Xephyr、不 pkill 其他进程。
#
# 用法：
#   cargo build -p tervo-app --bin tervo --bin xwin --bin xclick --bin xdrop
#   ./scripts/e2e_terminal_ui.sh
#
# 依赖：Xvfb、OpenBox（仓库 .perf/）、xwd、ffmpeg、python3 + Pillow（像素检测）。
# 环境变量：E2E_BIN、E2E_DISPLAY、TERVO_PERF_DIR、E2E_XVFB_BIN。
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT=$PWD
OUT=${E2E_OUT:-$ROOT/.logs/ui-basics}
mkdir -p "$OUT" "$OUT/obhome" /tmp/.X11-unix

BIN=${E2E_BIN:-$ROOT/target/debug/tervo}
XWIN=$ROOT/target/debug/xwin
XCLICK=$ROOT/target/debug/xclick
XDROP=$ROOT/target/debug/xdrop
for f in "$BIN" "$XWIN" "$XCLICK" "$XDROP"; do
    [ -x "$f" ] || { echo "缺少 $f；先 cargo build -p tervo-app --bin tervo --bin xwin --bin xclick --bin xdrop" >&2; exit 2; }
done

COMMON=$(git rev-parse --git-common-dir)
MAIN_ROOT=$(cd "$(dirname "$COMMON")" && pwd)
PERF=${TERVO_PERF_DIR:-$MAIN_ROOT/.perf}
XVFB_BIN=${E2E_XVFB_BIN:-$PERF/xvfb/usr/bin/Xvfb}
[ -x "$XVFB_BIN" ] || XVFB_BIN=$(command -v Xvfb || true)
[ -x "$XVFB_BIN" ] || { echo "找不到 Xvfb；不会回退到 Xephyr" >&2; exit 2; }
OPENBOX=$PERF/openbox/usr/bin/openbox
OPENBOX_LIB=$PERF/openbox/usr/lib/x86_64-linux-gnu

DISPLAY_NUM=${E2E_DISPLAY:-:115}
export DISPLAY=$DISPLAY_NUM
export HOME=$OUT/obhome
export XMODIFIERS=@im=none
export SLINT_BACKEND=winit-software

if xdpyinfo >/dev/null 2>&1; then
    echo "display $DISPLAY_NUM 已被占用；为避免干扰其他会话，本脚本退出。" >&2
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
"$XVFB_BIN" "$DISPLAY_NUM" -screen 0 1280x800x24 -ac -nolisten tcp >"$OUT/xvfb.log" 2>&1 &
XVFB_PID=$!
for _ in $(seq 1 100); do xdpyinfo >/dev/null 2>&1 && break; sleep 0.05; done
xdpyinfo >/dev/null 2>&1 || { echo "Xvfb 启动失败：$OUT/xvfb.log" >&2; exit 2; }

if [ -x "$OPENBOX" ]; then
    LD_LIBRARY_PATH=$OPENBOX_LIB "$OPENBOX" >"$OUT/openbox.log" 2>&1 &
    OB_PID=$!
    sleep 0.6
else
    echo "提示：未找到仓库 .perf/openbox，继续无 WM 运行（窗口位置/焦点可能不同）"
fi

stop_app() {
    [ -n "$APP_PID" ] && kill "$APP_PID" 2>/dev/null || true
    wait "$APP_PID" 2>/dev/null || true
    APP_PID=""
    sleep 0.3
}

# start_app <tag> <config contents> <shell args...>
start_app() {
    local tag=$1 cfg=$2; shift 2
    local shell=${E2E_APP_SHELL:-/bin/sh}
    printf '%s\n' "$cfg" > "$OUT/$tag.toml"
    TERVO_CONFIG="$OUT/$tag.toml" TERVO_LOG_DIR="$OUT/logs" \
        "$BIN" --cols 80 --rows 20 --shell "$shell" -- "$@" >"$OUT/app_$tag.log" 2>&1 &
    APP_PID=$!
    WIN=""
    for _ in $(seq 1 100); do
        WIN=$(xwininfo -root -tree 2>/dev/null | grep -i '"tervo"' | head -1 | awk '{print $1}' || true)
        [ -n "$WIN" ] && break
        sleep 0.05
    done
    [ -n "$WIN" ] || { echo "找不到 Tervo 窗口（$tag）；见 $OUT/app_$tag.log" >&2; exit 2; }
    "$XWIN" Tervo 20 20 >/dev/null 2>&1 || true
    sleep 0.5
    WIN=$(xwininfo -root -tree | grep -i '"tervo"' | head -1 | awk '{print $1}')
    ABS_X=$(xwininfo -id "$WIN" | awk '/Absolute upper-left X/{print $NF}')
    ABS_Y=$(xwininfo -id "$WIN" | awk '/Absolute upper-left Y/{print $NF}')
    W=$(xwininfo -id "$WIN" | awk '/Width:/{print $2}')
    H=$(xwininfo -id "$WIN" | awk '/Height:/{print $2}')
    echo "  window=$WIN at $ABS_X,$ABS_Y ${W}x${H} log=$OUT/app_$tag.log"
}

shot() {
    local name=$1
    xwd -silent -id "$WIN" -out "$OUT/$name.xwd"
    ffmpeg -y -hide_banner -loglevel error -f xwd_pipe -i "$OUT/$name.xwd" "$OUT/$name.png"
    rm -f "$OUT/$name.xwd"
}

# ---------------------------------------------------------------- 1. 滚动条
echo "== 1) 滚动条 =="
start_app scrollbar 'theme = "tokyo-night"
font_size = 15
confirm_close = false' -c 'seq 1 300; sleep 600'
sleep 1
shot scrollbar_bottom
CX=$((ABS_X + W - 8))
CY=$((ABS_Y + 32 + 40))
"$XCLICK" "$CX" "$CY" >/dev/null
sleep 0.6
shot scrollbar_top
"$XCLICK" "$CX" "$((ABS_Y + 32 + 400))" drag "$CX" "$((ABS_Y + 32 + 100))" >/dev/null
sleep 0.7
shot scrollbar_drag
"$XCLICK" "$CX" "$((ABS_Y + H - 10))" >/dev/null
sleep 0.5
shot scrollbar_back_bottom
grep -q '\[ui\] scrollbar' "$OUT/app_scrollbar.log" || { echo "滚动条点击未触发" >&2; exit 1; }
if grep -q 'offset=0 history=0' "$OUT/app_scrollbar.log"; then echo "滚动条 history=0 异常" >&2; exit 1; fi
echo "  OK：点击/拖拽日志 $(grep -c '\[ui\] scrollbar' "$OUT/app_scrollbar.log") 条"
stop_app

# ---------------------------------------------------------------- 2. 视觉铃
echo "== 2) 视觉铃 =="
start_app bell 'theme = "tokyo-night"
font_size = 15
confirm_close = false
desktop_notifications = false' -c 'while true; do printf "\a"; sleep 0.4; done'
sleep 0.8
rm -rf "$OUT/bell_frames"; mkdir -p "$OUT/bell_frames"
ffmpeg -y -hide_banner -loglevel error -f x11grab -draw_mouse 0 -video_size "${W}x${H}" \
    -framerate 30 -i "$DISPLAY_NUM+${ABS_X},${ABS_Y}" -t 2 -vsync 0 "$OUT/bell_frames/f_%03d.png"
python3 - "$OUT" <<'PY'
import glob, sys
from PIL import Image
out = sys.argv[1]
hits = []
for f in sorted(glob.glob(out + "/bell_frames/f_*.png")):
    im = Image.open(f).convert("RGB")
    px = im.getpixel((120, 32))
    if px[2] > 180 and px[1] > 110 and px[0] < 180 and (px[2] - px[0]) > 40:
        hits.append(f)
print(f"  accent-border frames: {len(hits)}")
if not hits:
    sys.exit(1)
Image.open(hits[len(hits)//2]).save(out + "/bell_flash.png")
PY
grep -q 'bell=true' "$OUT/app_bell.log" || { echo "未收到 BEL 事件" >&2; exit 1; }
echo "  OK：$OUT/bell_flash.png"
stop_app

# ---------------------------------------------------------------- 3. 拖放
echo "== 3) 文件拖放 =="
DROP_DIR=/tmp/tervo-e2e-drop
rm -rf "$DROP_DIR"; mkdir -p "$DROP_DIR/dir with space"
printf 'x\n' > "$DROP_DIR/file one.txt"
printf 'y\n' > "$DROP_DIR/it's.txt"
E2E_APP_SHELL=/bin/bash start_app drop 'theme = "tokyo-night"
font_size = 15
confirm_close = false
desktop_notifications = false' --norc -i
sleep 1
# 用真实指针位置决定目标 pane（native 桥接通过 QueryPointer 命中）
"$XCLICK" "$((ABS_X + 300))" "$((ABS_Y + 200))" >/dev/null
sleep 0.2
"$XDROP" "$WIN" 300 168 "$DROP_DIR/file one.txt" "$DROP_DIR/dir with space" "$DROP_DIR/it's.txt" \
    >"$OUT/xdrop.log" 2>&1 &
XPID=$!
sleep 0.4
shot drop_hover
wait "$XPID"
sleep 0.8
shot drop_after
grep -q 'native drop pane=' "$OUT/app_drop.log" || { echo "原生 Drop 未触发" >&2; exit 1; }
grep -q 'bracketed=true' "$OUT/app_drop.log" || { echo "未按 bracketed paste 插入" >&2; exit 1; }
echo "  OK：$OUT/drop_hover.png / $OUT/drop_after.png"
stop_app
rm -rf "$DROP_DIR"

# ------------------------------------------------------- 4. 反向：关掉铃 / 无历史
echo "== 4) 反向验证 =="
start_app bell_off 'theme = "tokyo-night"
font_size = 15
confirm_close = false
desktop_notifications = false
visual_bell = false' -c 'while true; do printf "\a"; sleep 0.4; done'
sleep 0.8
rm -rf "$OUT/bell_off_frames"; mkdir -p "$OUT/bell_off_frames"
ffmpeg -y -hide_banner -loglevel error -f x11grab -draw_mouse 0 -video_size "${W}x${H}" \
    -framerate 30 -i "$DISPLAY_NUM+${ABS_X},${ABS_Y}" -t 1.5 -vsync 0 "$OUT/bell_off_frames/f_%03d.png"
python3 - "$OUT" <<'PY'
import glob, sys
from PIL import Image
out = sys.argv[1]
hits = 0
for f in sorted(glob.glob(out + "/bell_off_frames/f_*.png")):
    im = Image.open(f).convert("RGB")
    px = im.getpixel((120, 32))
    if px[2] > 180 and px[1] > 110 and px[0] < 180 and (px[2] - px[0]) > 40:
        hits += 1
print(f"  visual_bell=false accent frames: {hits}")
sys.exit(0 if hits == 0 else 1)
PY
grep -q 'bell=true' "$OUT/app_bell_off.log" || { echo "未收到 BEL（反向用例）" >&2; exit 1; }
stop_app

start_app nohist 'theme = "tokyo-night"
font_size = 15
confirm_close = false' -c 'sleep 600'
sleep 1
shot no_history
python3 - "$OUT/no_history.png" <<'PY'
import sys
from PIL import Image
im = Image.open(sys.argv[1]).convert("RGB")
found = 0
for x in range(im.width - 12, im.width - 2):
    for y in range(40, im.height - 10, 4):
        p = im.getpixel((x, y))
        if p[2] > 120 and (p[2] - p[0]) > 25:
            found += 1
print(f"  no-history scrollbar pixels: {found}")
sys.exit(0 if found == 0 else 1)
PY
stop_app

echo "全部通过。截图在 $OUT"
