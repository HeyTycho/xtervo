#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
# Settings UI 视觉验收：在 headless Xvfb :124 上启动 xtervo，截图覆盖
#   open / 分类 / 下拉 / 搜索跳转 / 关闭 / 小窗口 / legacy 回退。
# 只 kill 自己启动的 Xvfb/Openbox/app；display 被占用时直接退出。
#
# 用法：
#   cargo build -p xtervo-app --bin xtervo --bin xwin --bin xclick --bin xkey
#   ./scripts/settings_ui_shots.sh
# 依赖：Xvfb、Openbox、xwd、ffmpeg（仓库 .perf/ 已带 Xvfb/Openbox）。
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT=$PWD
OUT=${SETTINGS_SHOTS_OUT:-$ROOT/.logs/settings-ui/shots}
mkdir -p "$OUT/home" "$OUT/logs" /tmp/.X11-unix

BIN=$ROOT/target/debug/xtervo
XWIN=$ROOT/target/debug/xwin
XCLICK=$ROOT/target/debug/xclick
XKEY=$ROOT/target/debug/xkey
for f in "$BIN" "$XWIN" "$XCLICK" "$XKEY"; do
    [ -x "$f" ] || { echo "缺少 $f；先 cargo build -p xtervo-app --bin xtervo --bin xwin --bin xclick --bin xkey" >&2; exit 2; }
done

COMMON=$(git rev-parse --git-common-dir)
MAIN_ROOT=$(cd "$(dirname "$COMMON")" && pwd)
PERF=${XTERVO_PERF_DIR:-$MAIN_ROOT/.perf}
XVFB_BIN=${E2E_XVFB_BIN:-$PERF/xvfb/usr/bin/Xvfb}
[ -x "$XVFB_BIN" ] || XVFB_BIN=$(command -v Xvfb || true)
[ -x "$XVFB_BIN" ] || { echo "找不到 Xvfb；不会回退到 Xephyr" >&2; exit 2; }
OPENBOX=$PERF/openbox/usr/bin/openbox
OPENBOX_LIB=$PERF/openbox/usr/lib/x86_64-linux-gnu

DISPLAY_NUM=${E2E_DISPLAY:-:124}
export DISPLAY=$DISPLAY_NUM
export HOME=$OUT/home
export XMODIFIERS=@im=none
export SLINT_BACKEND=winit-software

if xdpyinfo >/dev/null 2>&1; then
    echo "display $DISPLAY_NUM 已被占用；先停掉占用它的会话。" >&2
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

"$XVFB_BIN" "$DISPLAY_NUM" -screen 0 1600x1000x24 -ac -nolisten tcp >"$OUT/xvfb.log" 2>&1 &
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
start_app() {  # <tag> [XTERVO_SETTINGS_LEGACY]
    local tag=$1 legacy=${2:-}
    local log="$OUT/app_$tag.log"
    : >"$log"
    printf '%s\n' 'theme = "tokyo-night"' 'font_size = 15' 'confirm_close = false' > "$OUT/$tag.toml"
    XTERVO_SETTINGS_LEGACY="$legacy" XTERVO_CONFIG="$OUT/$tag.toml" XTERVO_LOG_DIR="$OUT/logs" \
        "$BIN" --cols 120 --rows 32 --shell /bin/sh -- -c 'sleep 900' >"$log" 2>&1 &
    APP_PID=$!
    WIN=""
    for _ in $(seq 1 160); do
        WIN=$(xwininfo -root -tree 2>/dev/null | grep -i '"xtervo"' | head -1 | awk '{print $1}' || true)
        [ -n "$WIN" ] && break
        sleep 0.05
    done
    [ -n "$WIN" ] || { echo "启动失败（$tag）：$log" >&2; exit 2; }
    "$XWIN" xtervo 20 20 >/dev/null 2>&1 || true
    sleep 0.8
    WIN=$(xwininfo -root -tree | grep -i '"xtervo"' | head -1 | awk '{print $1}')
    ABS_X=$(xwininfo -id "$WIN" | awk '/Absolute upper-left X/{print $NF}')
    ABS_Y=$(xwininfo -id "$WIN" | awk '/Absolute upper-left Y/{print $NF}')
    W=$(xwininfo -id "$WIN" | awk '/Width:/{print $2}')
    H=$(xwininfo -id "$WIN" | awk '/Height:/{print $2}')
    if [ "$W" != 1080 ] || [ "$H" != 736 ]; then
        echo "截图坐标基于 1080x736（cell 9x22 / 120x32），实际 ${W}x${H}" >&2
        exit 2
    fi
    echo "  window=$WIN ${W}x${H} legacy=${legacy:-none}"
}

refresh_geom() {
    WIN=$(xwininfo -root -tree | grep -i '"xtervo"' | head -1 | awk '{print $1}')
    ABS_X=$(xwininfo -id "$WIN" | awk '/Absolute upper-left X/{print $NF}')
    ABS_Y=$(xwininfo -id "$WIN" | awk '/Absolute upper-left Y/{print $NF}')
    W=$(xwininfo -id "$WIN" | awk '/Width:/{print $2}')
    H=$(xwininfo -id "$WIN" | awk '/Height:/{print $2}')
    echo "  window=${W}x${H}"
}

stop_app() {
    [ -n "$APP_PID" ] && kill "$APP_PID" 2>/dev/null || true
    wait "$APP_PID" 2>/dev/null || true
    APP_PID=""
    sleep 0.4
}

shot() {
    local name=$1
    xwd -silent -id "$WIN" -out "$OUT/$name.xwd"
    ffmpeg -y -hide_banner -loglevel error -f xwd_pipe -i "$OUT/$name.xwd" "$OUT/$name.png"
    rm -f "$OUT/$name.xwd"
    echo "  shot $OUT/$name.png"
}

gear() {
    "$XCLICK" "$((ABS_X + W - 118))" "$((ABS_Y + 16))" >/dev/null
    sleep 0.7
}

start_app layout
gear
shot settings_open
# 主题下拉（Appearance 首屏 Color scheme）
"$XCLICK" "$((ABS_X + 620))" "$((ABS_Y + 198))" >/dev/null
sleep 0.5
shot settings_dropdown
# 收起下拉，切 Terminal
"$XCLICK" "$((ABS_X + 100))" "$((ABS_Y + 174))" >/dev/null
sleep 0.6
shot settings_terminal
# 搜索 + 跳转（合成键盘前显式激活窗口）
"$XWIN" xtervo >/dev/null 2>&1 || true
"$XCLICK" "$((ABS_X + 800))" "$((ABS_Y + 56))" >/dev/null
sleep 0.3
"$XKEY" sixel >/dev/null 2>&1 || true
sleep 0.6
shot settings_search
"$XCLICK" "$((ABS_X + 300))" "$((ABS_Y + 148))" >/dev/null
sleep 0.9
shot settings_search_jump
# Profiles 分类（带图标分类/空态 + Manage 入口）
"$XCLICK" "$((ABS_X + 100))" "$((ABS_Y + 320))" >/dev/null
sleep 0.6
shot settings_profiles
# Esc 关闭
"$XWIN" xtervo >/dev/null 2>&1 || true
"$XKEY" --key escape >/dev/null 2>&1 || true
sleep 0.6
shot settings_closed
stop_app

# 小窗口：拖右边缘/下边缘缩小到约 700x500，再缩到 480x360。
start_app small
gear
"$XCLICK" "$((ABS_X + W - 1))" "$((ABS_Y + 300))" drag "$((ABS_X + 699))" "$((ABS_Y + 300))" >/dev/null
sleep 0.6
"$XCLICK" "$((ABS_X + 500))" "$((ABS_Y + H - 1))" drag "$((ABS_X + 500))" "$((ABS_Y + 499))" >/dev/null
sleep 0.8
refresh_geom
shot settings_small
"$XCLICK" "$((ABS_X + W - 1))" "$((ABS_Y + 250))" drag "$((ABS_X + 479))" "$((ABS_Y + 250))" >/dev/null
sleep 0.6
"$XCLICK" "$((ABS_X + 300))" "$((ABS_Y + H - 1))" drag "$((ABS_X + 300))" "$((ABS_Y + 359))" >/dev/null
sleep 0.8
refresh_geom
shot settings_tiny
stop_app

# legacy 回退：XTERVO_SETTINGS_LEGACY=1 -> 居中 modal（同一套控件）
start_app legacy 1
gear
shot settings_legacy
"$XCLICK" "$((ABS_X + 120))" "$((ABS_Y + 194))" >/dev/null
sleep 0.6
shot settings_legacy_terminal
stop_app

echo "全部截图在 $OUT"
