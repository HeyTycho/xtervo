#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
# 终端基础行为 e2e：可听铃 / 滚动策略 / 滚动条策略 / Backspace・Delete 编码 / 设置页。
#
# 在独立 headless Xvfb 上运行（默认 :118），只 kill 本脚本启动的 Xvfb/OpenBox/app，
# 不占用 :1 / 其它 display，也不 pkill 其他进程。
#
# 用法：
#   cargo build -p tervo-app --bin tervo --bin xwin --bin xclick --bin xkey
#   ./scripts/e2e_terminal_behavior.sh
#
# 依赖：Xvfb、OpenBox（仓库 .perf/）、xwd、ffmpeg、python3 + Pillow。
# 环境变量：E2E_BIN、E2E_DISPLAY、TERVO_PERF_DIR、E2E_XVFB_BIN。
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT=$PWD
OUT=${E2E_OUT:-$ROOT/.logs/term-behavior}
mkdir -p "$OUT" "$OUT/obhome" /tmp/.X11-unix

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

DISPLAY_NUM=${E2E_DISPLAY:-:118}
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

# start_app <tag> <config contents> [--big] <shell args...>
# --big = 120x32（1080x736）；设置页坐标测试需要稳定的大窗口。
start_app() {
    local tag=$1 cfg=$2; shift 2
    local cols=80 rows=20
    if [ "${1:-}" = "--big" ]; then
        cols=120
        rows=32
        shift
    fi
    printf '%s\n' "$cfg" > "$OUT/$tag.toml"
    TERVO_CONFIG="$OUT/$tag.toml" TERVO_LOG_DIR="$OUT/logs" TERVO_KEYLOG=1 \
        "$BIN" --cols "$cols" --rows "$rows" --shell /bin/sh -- "$@" >"$OUT/app_$tag.log" 2>&1 &
    APP_PID=$!
    WIN=""
    for _ in $(seq 1 120); do
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

focus_pane() {
    # OpenBox 激活窗口 + 点击 pane；确认 _NET_ACTIVE_WINDOW 已切到本窗口再发键，
    # 避免 Xvfb/OpenBox 启动较慢时按键落到 root/上个进程。
    "$XWIN" Tervo >/dev/null 2>&1 || true
    "$XCLICK" "$((ABS_X + W / 2))" "$((ABS_Y + 32 + (H - 32) / 2))" >/dev/null
    for _ in $(seq 1 40); do
        local active
        active=$(xprop -root _NET_ACTIVE_WINDOW 2>/dev/null | awk '{print $NF}')
        [ "$active" = "$WIN" ] && break
        sleep 0.05
    done
    sleep 0.5
}

# ------------------------------------------------- 1. 默认 Backspace/Delete 编码
echo "== 1) 默认编码（GNOME：Backspace=0x7f，Delete=ESC [3~） =="
start_app key_default 'theme = "tokyo-night"
font_size = 15
confirm_close = false' -c 'sleep 600'
focus_pane
"$XKEY" --key backspace >/dev/null
"$XKEY" --key delete >/dev/null
sleep 0.4
grep -Fq '[send] pane=' "$OUT/app_key_default.log" || { echo "没有按键发送日志" >&2; exit 1; }
grep -Fq 'bytes=[127]' "$OUT/app_key_default.log" || { echo "默认 Backspace 不是 0x7f" >&2; exit 1; }
grep -Fq 'bytes=[27, 91, 51, 126]' "$OUT/app_key_default.log" || { echo "默认 Delete 不是 ESC [3~" >&2; exit 1; }
echo "  OK：Backspace=0x7f，Delete=ESC [3~"
stop_app

# ------------------------------------------------- 2. 可配置 Backspace/Delete
echo "== 2) 自定义编码（control-h / ascii-del） =="
start_app key_custom 'theme = "tokyo-night"
font_size = 15
confirm_close = false
backspace_binding = "control-h"
delete_binding = "ascii-del"' -c 'sleep 600'
focus_pane
"$XKEY" --key backspace >/dev/null
"$XKEY" --key delete >/dev/null
sleep 0.4
grep -Fq 'bytes=[8]' "$OUT/app_key_custom.log" || { echo "control-h Backspace 不是 0x08" >&2; exit 1; }
grep -Fq 'bytes=[127]' "$OUT/app_key_custom.log" || { echo "ascii-del Delete 不是 0x7f" >&2; exit 1; }
echo "  OK：Backspace=0x08，Delete=0x7f"
stop_app

# --------------------------------------------------------- 3. 可听铃与视觉铃独立
echo "== 3) audible bell =="
start_app audible 'theme = "tokyo-night"
font_size = 15
confirm_close = false
desktop_notifications = false
visual_bell = false
audible_bell = true' -c 'while true; do printf "\a"; sleep 0.25; done'
sleep 1.4
grep -q 'audible count=' "$OUT/app_audible.log" || { echo "audible_bell=true 没有响铃日志" >&2; exit 1; }
grep -Fq 'backend=ok' "$OUT/app_audible.log" || { echo "X11 响铃后端没有成功" >&2; exit 1; }
echo "  OK：X11 Bell 已调用（visual_bell=false 不影响）"
stop_app

# ------------------------------------------- 4. scrollbar_policy always/auto/never
echo "== 4) scrollbar policy（无历史） =="
check_scrollbar() {
    local name=$1 expect=$2
    python3 - "$OUT/$name.png" "$expect" <<'PY'
import sys
from PIL import Image
path, expect = sys.argv[1], sys.argv[2]
im = Image.open(path).convert("RGB")
w, h = im.size
x = w - 8            # overlay thumb 中心列（pane 右侧 12px 起点 + 2px 内缩）
ref_x = w - 40       # 同高度终端背景（远离滚动条）
hits = 0
for y in range(40, h - 6, 2):
    p = im.getpixel((x, y))
    q = im.getpixel((ref_x, y))
    if max(abs(p[i] - q[i]) for i in range(3)) > 8:
        hits += 1
total = len(range(40, h - 6, 2))
print(f"  {path.rsplit('/', 1)[-1]} thumb_pixels={hits}/{total}")
if expect == "visible":
    sys.exit(0 if hits > total * 0.8 else 1)
sys.exit(0 if hits == 0 else 1)
PY
}

start_app sb_always 'theme = "tokyo-night"
font_size = 15
confirm_close = false
scrollbar_policy = "always"' -c 'sleep 600'
sleep 0.8
shot sb_always
check_scrollbar sb_always visible || { echo "always 策略没有画滚动条" >&2; exit 1; }
stop_app

start_app sb_auto 'theme = "tokyo-night"
font_size = 15
confirm_close = false
scrollbar_policy = "auto"' -c 'sleep 600'
sleep 0.8
shot sb_auto
check_scrollbar sb_auto hidden || { echo "auto 无历史时不应画滚动条" >&2; exit 1; }
stop_app

start_app sb_never 'theme = "tokyo-night"
font_size = 15
confirm_close = false
scrollbar_policy = "never"' -c 'seq 1 400; sleep 600'
sleep 1.0
shot sb_never
check_scrollbar sb_never hidden || { echo "never 有历史时也不应画滚动条" >&2; exit 1; }
stop_app
echo "  OK：always 显示、auto 无历史隐藏、never 始终隐藏"

# --------------------------------------------------------- 5. 设置页 Terminal 控件
echo "== 5) 设置 Tab Terminal 控件 =="
# 新设置页是完整 Settings Tab：固定 1080x736 窗口，坐标与响应式侧栏都可预测。
start_app settings 'theme = "tokyo-night"
font_size = 15
confirm_close = false' --big -c 'sleep 600'
sleep 0.5
[ "$W" = 1080 ] && [ "$H" = 736 ] || {
    echo "设置页坐标基于 1080x736（cell 9x22 / 120x32），实际 ${W}x${H}；请检查字体回退" >&2
    exit 2
}
# 齿轮：root 右上角 x = W-132+14，y = 4+12
"$XWIN" Tervo >/dev/null 2>&1 || true
"$XCLICK" "$((ABS_X + W - 118))" "$((ABS_Y + 16))" >/dev/null
sleep 0.8
grep -q '\[ui\] settings open=true' "$OUT/app_settings.log" || { echo "齿轮没有打开设置" >&2; exit 1; }

# 主题下拉：首屏 Appearance 的 Color scheme 触发区（window 620,198）
"$XCLICK" "$((ABS_X + 620))" "$((ABS_Y + 198))" >/dev/null
sleep 0.5
shot settings_theme_dropdown

# 左侧导航 Terminal（window 100,174）
"$XCLICK" "$((ABS_X + 100))" "$((ABS_Y + 174))" >/dev/null
sleep 0.6
shot settings_terminal_top
grep -q '\[ui\] settings category=1' "$OUT/app_settings.log" || { echo "没有切到 Terminal 设置分类" >&2; exit 1; }

# Unlimited scrollback 开关（首屏，switch 中心 window 1025,303）
"$XCLICK" "$((ABS_X + 1025))" "$((ABS_Y + 303))" >/dev/null
sleep 0.4
grep -Fq 'scrollback_unlimited = true' "$OUT/settings.toml" || { echo "设置页没有写入 scrollback_unlimited" >&2; exit 1; }
grep -q '\[ui\] scrollback_unlimited=true' "$OUT/app_settings.log" || { echo "设置页没有触发 unlimited 回调" >&2; exit 1; }
shot settings_terminal_unlimited
# 再点一次恢复固定值，避免影响后续断言
"$XCLICK" "$((ABS_X + 1025))" "$((ABS_Y + 303))" >/dev/null
sleep 0.3
grep -Fq 'scrollback_unlimited = false' "$OUT/settings.toml" || { echo "unlimited 开关没有恢复 false" >&2; exit 1; }

# Scrollbar Always（首屏 Scrolling 卡片，window 378,550）——Sixel Images 段之前
# 的位置已经失效；新页把 scrollbar 放在第二张卡片，首屏即可点。
"$XCLICK" "$((ABS_X + 378))" "$((ABS_Y + 550))" >/dev/null
sleep 0.4
grep -Fq 'scrollbar_policy = "always"' "$OUT/settings.toml" || { echo "设置页没有写入 scrollbar_policy=always" >&2; exit 1; }
grep -q '\[ui\] scrollbar_policy=always' "$OUT/app_settings.log" || { echo "设置页没有触发 scrollbar_policy 回调" >&2; exit 1; }

# Scroll on output（同一张 Scrolling 卡片，window 1025,430）
"$XCLICK" "$((ABS_X + 1025))" "$((ABS_Y + 430))" >/dev/null
sleep 0.4
grep -Fq 'scroll_on_output = true' "$OUT/settings.toml" || { echo "设置页没有写入 scroll_on_output" >&2; exit 1; }
grep -q '\[ui\] scroll_on_output=true' "$OUT/app_settings.log" || { echo "设置页没有触发 scroll_on_output 回调" >&2; exit 1; }
shot settings_terminal_mid

# 滚到底：Bell / Images / Key encoding 的相对位置固定
"$XWIN" Tervo >/dev/null 2>&1 || true
for _ in $(seq 1 12); do
    "$XCLICK" "$((ABS_X + 600))" "$((ABS_Y + 400))" 5 >/dev/null
    sleep 0.05
done
sleep 0.5
shot settings_terminal_scroll

# Audible bell（底部 Bell 卡片第二行 switch，window 1025,350）
"$XCLICK" "$((ABS_X + 1025))" "$((ABS_Y + 311))" >/dev/null
sleep 0.4
grep -Fq 'audible_bell = true' "$OUT/settings.toml" || { echo "设置页没有写入 audible_bell" >&2; exit 1; }
grep -q '\[ui\] audible_bell=true' "$OUT/app_settings.log" || { echo "设置页没有触发 audible_bell 回调" >&2; exit 1; }

# Backspace Control-H / Delete ASCII DEL（底部 Key encoding 两个分段控件）
"$XCLICK" "$((ABS_X + 855))" "$((ABS_Y + 593))" >/dev/null
sleep 0.4
grep -Fq 'backspace_binding = "control-h"' "$OUT/settings.toml" || { echo "设置页没有写入 backspace_binding" >&2; exit 1; }
"$XCLICK" "$((ABS_X + 650))" "$((ABS_Y + 660))" >/dev/null
sleep 0.4
grep -Fq 'delete_binding = "ascii-del"' "$OUT/settings.toml" || { echo "设置页没有写入 delete_binding" >&2; exit 1; }

# 搜索跳转：输入 sixel，点第一条结果，应切到 Terminal 并滚到 Images 卡片。
"$XCLICK" "$((ABS_X + 800))" "$((ABS_Y + 56))" >/dev/null
sleep 0.3
"$XKEY" sixel >/dev/null 2>&1 || true
sleep 0.6
grep -q '\[ui\] settings filter q="sixel" hits=1' "$OUT/app_settings.log" || { echo "设置搜索没有命中 sixel" >&2; exit 1; }
shot settings_search
"$XCLICK" "$((ABS_X + 300))" "$((ABS_Y + 148))" >/dev/null
sleep 0.9
grep -q '\[ui\] settings goto cat=1' "$OUT/app_settings.log" || { echo "搜索结果没有跳到 Terminal" >&2; exit 1; }
grep -q '\[ui\] settings jump section=images' "$OUT/app_settings.log" || { echo "搜索结果没有跳到 Images 卡片" >&2; exit 1; }
shot settings_search_jump

# Esc 关闭设置；Rust 侧应记录 open=false 并归还终端焦点。
"$XKEY" --key escape >/dev/null 2>&1 || true
sleep 0.6
grep -q '\[ui\] settings open=false' "$OUT/app_settings.log" || { echo "Esc 没有关闭设置" >&2; exit 1; }
shot settings_closed

echo "  OK：$OUT/settings_terminal_top.png / _mid.png / _scroll.png / _unlimited.png / settings_theme_dropdown.png / settings_search_jump.png"
stop_app

echo "全部通过。截图/日志在 $OUT"
