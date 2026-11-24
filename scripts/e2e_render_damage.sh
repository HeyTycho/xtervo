#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
# P1-06 渲染 damage e2e：真实 GUI（Xvfb :137 + Openbox）截图 + XTERVO_RENDER_PROFILE 断言。
#
# 覆盖：
#   1. 单行输出/按键：full=false、rows=1、full_redraws 不增长；
#   2. 搜索高亮：只重绘命中行（full=false），截图含高亮色；
#   3. 滚动（100 行输出）：整屏 damage 走 full，截图不花屏；
#   4. resize：grid 变化触发 full，窗口变宽、内容完整；
#   5. 主题切换（外置 config 热更新）：full 重绘 + 背景色切到新主题。
#
# 只操作自己启动的 Xvfb/Openbox/xtervo；不占用 :1，不 pkill 其他进程。
#
# 用法：
#   CARGO_TARGET_DIR=$PWD/target ./scripts/e2e_render_damage.sh
#   E2E_BIN/E2E_XWIN/E2E_XCLICK/E2E_XKEY 可覆盖二进制路径。
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT=$PWD
OUT=${E2E_OUT:-$ROOT/.logs/render-damage}
DISPLAY_NUM=${E2E_DISPLAY:-:137}
TARGET_DIR=${CARGO_TARGET_DIR:-$ROOT/target}
BIN=${E2E_BIN:-$TARGET_DIR/debug/xtervo}
XWIN=${E2E_XWIN:-$TARGET_DIR/debug/xwin}
XCLICK=${E2E_XCLICK:-$TARGET_DIR/debug/xclick}
XKEY=${E2E_XKEY:-$TARGET_DIR/debug/xkey}

mkdir -p "$OUT" "$OUT/home" "$OUT/logs"
for f in "$BIN" "$XWIN" "$XCLICK" "$XKEY"; do
    [ -x "$f" ] || {
        echo "缺少 $f；先 cargo build -p xtervo-app --bin xtervo --bin xwin --bin xclick --bin xkey" >&2
        exit 2
    }
done
command -v xwd >/dev/null || { echo "缺少 xwd（x11-apps）" >&2; exit 2; }
command -v ffmpeg >/dev/null || { echo "缺少 ffmpeg" >&2; exit 2; }
command -v xdpyinfo >/dev/null || { echo "缺少 xdpyinfo（x11-utils）" >&2; exit 2; }

COMMON=$(git rev-parse --git-common-dir)
MAIN_ROOT=$(cd "$(dirname "$COMMON")" && pwd)
PERF=${XTERVO_PERF_DIR:-$MAIN_ROOT/.perf}
XVFB_BIN=${E2E_XVFB_BIN:-$PERF/xvfb/usr/bin/Xvfb}
[ -x "$XVFB_BIN" ] || XVFB_BIN=$(command -v Xvfb || true)
[ -x "$XVFB_BIN" ] || { echo "找不到 Xvfb；不会回退到 Xephyr" >&2; exit 2; }
OPENBOX=$PERF/openbox/usr/bin/openbox
OPENBOX_LIB=$PERF/openbox/usr/lib/x86_64-linux-gnu

export DISPLAY=$DISPLAY_NUM
export HOME=$OUT/home
export XMODIFIERS=@im=none
export SLINT_BACKEND=winit-software
export XTERVO_CONFIG=$OUT/config.toml
export XTERVO_LOG_DIR=$OUT/logs
export XTERVO_RENDER_PROFILE=1
export XTERVO_CURSOR_BLINK_PHASE=off

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

rm -rf "$OUT/logs" "$OUT"/*.png
mkdir -p "$OUT/logs"
export XTERVO_E2E_INPUT=$OUT/input.txt
rm -f "$XTERVO_E2E_INPUT"
cat > "$OUT/config.toml" <<'CFG'
theme = "tokyo-night"
font_size = 15
confirm_close = false
desktop_notifications = false
confirm_multiline_paste = false
CFG

# 受控 shell：启动打印 10 行（含 target），支持 do-scroll 输出 100 行。
cat > "$OUT/shell.sh" <<SHELL
#!/bin/sh
printf 'RENDER-READY\n'
printf 'alpha target one\n'
printf 'beta target two\n'
printf 'gamma three\n'
printf 'delta four\n'
printf 'epsilon five\n'
printf 'zeta six\n'
printf 'eta seven\n'
printf 'theta eight\n'
printf 'iota nine\n'
while IFS= read -r line; do
  printf '%s\n' "\$line" >> "\$XTERVO_E2E_INPUT"
  case "\$line" in
    do-scroll)
      i=1
      while [ "\$i" -le 100 ]; do
        printf 'scroll-%03d target\n' "\$i"
        i=\$((i + 1))
      done
      ;;
    *) printf 'OUT:%s\n' "\$line" ;;
  esac
done
SHELL
chmod +x "$OUT/shell.sh"

echo "== 启动 Xvfb $DISPLAY_NUM =="
"$XVFB_BIN" "$DISPLAY_NUM" -screen 0 1280x800x24 -ac -nolisten tcp >"$OUT/xvfb.log" 2>&1 &
XVFB_PID=$!
for _ in $(seq 1 100); do xdpyinfo >/dev/null 2>&1 && break; sleep 0.05; done
xdpyinfo >/dev/null 2>&1 || { echo "Xvfb 启动失败：$OUT/xvfb.log" >&2; exit 2; }

if [ -x "$OPENBOX" ]; then
    # 与 e2e_wm_resize 一致：仓库 .perf/openbox 需要自己的 data/lib，否则
    # Openbox 找不到主题/配置会退出或无法接管 WM，XTest 按键就送不到窗口。
    OB_DATA=$PERF/openbox/usr/share
    mkdir -p "$OUT/ob-home" "$OUT/ob-cache"
    env HOME="$OUT/ob-home" XDG_CACHE_HOME="$OUT/ob-cache" \
        XDG_DATA_DIRS="$OB_DATA" LD_LIBRARY_PATH="$OPENBOX_LIB" \
        "$OPENBOX" --sm-disable >"$OUT/openbox.log" 2>&1 &
    OB_PID=$!
    for _ in $(seq 1 100); do
        xprop -root _NET_SUPPORTING_WM_CHECK 2>/dev/null | grep -q 'window id' && break
        sleep 0.05
    done
    xprop -root _NET_SUPPORTING_WM_CHECK 2>/dev/null | grep -q 'window id' \
        || { echo "Openbox 未接管 $DISPLAY_NUM（$OUT/openbox.log）" >&2; exit 2; }
else
    echo "提示：未找到仓库 .perf/openbox，继续无 WM 运行" >&2
fi

echo "== 启动 xtervo =="
"$BIN" --cols 100 --rows 30 --title xtervo --shell /bin/sh -- "$OUT/shell.sh" \
    >"$OUT/app.log" 2>&1 &
APP_PID=$!

WIN=""
for _ in $(seq 1 200); do
    WIN=$(xwininfo -root -tree 2>/dev/null | grep -i '"xtervo"' | head -1 | awk '{print $1}' || true)
    [ -n "$WIN" ] && break
    sleep 0.05
done
[ -n "$WIN" ] || { echo "找不到 xtervo 窗口；见 $OUT/app.log" >&2; tail -40 "$OUT/app.log" >&2; exit 2; }
"$XWIN" xtervo 20 20 >/dev/null 2>&1 || true
sleep 1.5

LOG=$(ls -t "$OUT"/logs/xtervo-*.log 2>/dev/null | head -1)
[ -n "$LOG" ] || { echo "没有 xtervo 日志（$OUT/logs）" >&2; exit 2; }

geom() {
    xwininfo -id "$WIN" | awk '
        /Absolute upper-left X:/ {x=$NF}
        /Absolute upper-left Y:/ {y=$NF}
        /Width:/ {w=$2}
        /Height:/ {h=$2}
        END {print x, y, w, h}'
}
read -r ABS_X ABS_Y W H <<<"$(geom)"
echo "  window=$WIN at $ABS_X,$ABS_Y ${W}x${H}"
[ "$W" -ge 800 ] && [ "$H" -ge 600 ] || { echo "窗口尺寸异常：${W}x${H}" >&2; exit 2; }

fail() {
    echo "FAIL: $*" >&2
    echo "--- render log tail ---" >&2
    render_log | tail -20 >&2
    echo "--- app log tail ---" >&2
    tail -80 "$OUT/app.log" >&2
    exit 1
}

render_log() { grep -F '[render] pane=' "$LOG" || true; }
last_render() { render_log | tail -1; }
new_render_since() { render_log | tail -n +$(($1 + 1)); }
fld() { # <line> <key>
    printf '%s\n' "$1" | tr ' ' '\n' | awk -F= -v k="$2" '$1==k{print $2; exit}'
}
wait_render_after() { # <lines-before> <timeout-ticks>
    local before=$1 ticks=${2:-60}
    for _ in $(seq 1 "$ticks"); do
        [ "$(render_log | wc -l)" -gt "$before" ] && return 0
        sleep 0.05
    done
    return 1
}
shot() {
    xwd -silent -id "$WIN" -out "$OUT/$1.xwd"
    ffmpeg -y -hide_banner -loglevel error -f xwd_pipe -i "$OUT/$1.xwd" "$OUT/$1.png"
    rm -f "$OUT/$1.xwd"
}
key() { "$XKEY" "$@" >/dev/null 2>&1; sleep 0.15; }
click() { "$XCLICK" "$@" >/dev/null 2>&1; sleep 0.2; }

# 焦点落到终端内容区（避开 tabbar 32px 与右侧滚动条）。
# Xvfb + Openbox 下首击偶尔只完成 map/focus 转换：轮询 _NET_ACTIVE_WINDOW，
# 未拿到焦点就再点一次，避免把“按键没送达”误判成 damage 失效。
active_win() { xprop -root _NET_ACTIVE_WINDOW 2>/dev/null | grep -o '0x[0-9a-f]\+' | head -1; }
focused=0
for _ in $(seq 1 40); do
    click $((ABS_X + 200)) $((ABS_Y + 200))
    if [ "$(active_win)" = "$WIN" ]; then focused=1; break; fi
    sleep 0.05
done
[ "$focused" = "1" ] || fail "窗口未获得键盘焦点（_NET_ACTIVE_WINDOW=$(active_win), win=$WIN）"
sleep 0.5

[ -n "$(last_render)" ] || fail "启动后没有 [render] profile 行"
start_line=$(last_render)
start_full=$(fld "$start_line" full_redraws)
echo "  启动 profile: $start_line"
[ -n "$start_full" ] || fail "profile 行没有 full_redraws 字段"

# ---------------------------------------------------------------- 1. 单行输出
echo "== 1) 单行输出/连续按键 =="
before_count=$(render_log | wc -l)
for ch in a b c d e f; do
    "$XKEY" "$ch" >/dev/null 2>&1
    sleep 0.18
done
wait_render_after "$before_count" 80 || fail "按键后没有新的 render 行"
sleep 0.3
line=$(last_render)
echo "  按键后 profile: $line"
[ "$(fld "$line" full)" = "false" ] || fail "单行按键触发了整屏：$line"
rows=$(fld "$line" rows)
[ -n "$rows" ] && [ "$rows" -le 2 ] || fail "单行按键重绘行数异常 rows=$rows"
[ "$(fld "$line" full_redraws)" = "$start_full" ] || fail "full_redraws 随按键增长：$start_full -> $(fld "$line" full_redraws)"
shot 01-single-line
[ -s "$OUT/01-single-line.png" ] || fail "单行截图缺失"
# 提交刚输入的一行，避免它和下一步的 do-scroll 拼成同一行。
key --key enter
sleep 0.5

# ---------------------------------------------------------------- 2. 搜索高亮
echo "== 2) 搜索高亮 =="
before_count=$(render_log | wc -l)
key f --ctrl --shift
sleep 0.3
"$XKEY" target >/dev/null 2>&1
sleep 0.2
key --key enter
sleep 0.6
wait_render_after "$before_count" 60 || fail "搜索后没有新的 render 行"
grep -Fq '[ui] find query="target"' "$OUT/app.log" || fail "搜索未执行（日志缺少 find query=target）"
line=$(last_render)
echo "  搜索后 profile: $line"
[ "$(fld "$line" full)" = "false" ] || fail "视口内搜索命中不应整屏：$line"
[ "$(fld "$line" rows)" -ge 1 ] || fail "搜索高亮没有重绘命中行"
shot 02-search-highlight
python3 - "$OUT/02-search-highlight.png" <<'PY'
import sys
from PIL import Image
img = Image.open(sys.argv[1]).convert("RGB")
n = sum(1 for p in img.getdata() if p == (249, 226, 175))
if n < 100:
    print(f"FAIL: 搜索高亮像素只有 {n}", file=sys.stderr)
    sys.exit(1)
print(f"  search highlight pixels={n}")
PY
key --key escape
sleep 0.3
click $((ABS_X + 200)) $((ABS_Y + 200))
sleep 0.4

# ---------------------------------------------------------------- 3. 滚动
echo "== 3) 滚动 100 行 =="
before_count=$(render_log | wc -l)
"$XKEY" do-scroll >/dev/null 2>&1
sleep 0.3
key --key enter
sleep 1.2
wait_render_after "$before_count" 80 || fail "滚动后没有新的 render 行"
sleep 0.3
# 滚动期间至少有一帧整屏（full=true，rows=面板行数）；允许最后一帧是局部。
if ! new_render_since "$before_count" | grep -q 'full=true'; then
    echo "  shell 收到: $(tr '\n' ',' < "$XTERVO_E2E_INPUT" 2>/dev/null)" >&2
    echo "  最新 render 行:" >&2
    new_render_since "$before_count" | tail -8 >&2
    fail "滚动没有产生整屏重绘"
fi
shot 03-scroll
python3 - "$OUT/03-scroll.png" <<'PY'
import sys
from PIL import Image
img = Image.open(sys.argv[1]).convert("RGB")
colors = img.getcolors(maxcolors=1 << 24) or []
if not colors:
    print("FAIL: 空截图", file=sys.stderr)
    sys.exit(1)
print(f"  scroll screenshot colors={len(colors)}")
PY

# ---------------------------------------------------------------- 4. resize
echo "== 4) resize =="
before_count=$(render_log | wc -l)
click $((ABS_X + W - 2)) $((ABS_Y + H / 2)) drag $((ABS_X + W - 2 + 200)) $((ABS_Y + H / 2))
sleep 1.0
W2=0
for _ in $(seq 1 60); do
    read -r ABS_X2 ABS_Y2 W2 H2 <<<"$(geom)"
    [ "$W2" -ge $((W + 150)) ] && break
    sleep 0.05
done
echo "  resize 后 ${W2}x${H2}"
[ "$W2" -ge $((W + 150)) ] || fail "resize 未生效：$W -> $W2"
wait_render_after "$before_count" 80 || fail "resize 后没有新的 render 行"
sleep 0.3
new_render_since "$before_count" | grep -q 'full=true' || fail "resize 后没有整屏重绘"
shot 04-resize
# 旧帧残留检查：终端内容区不能出现大面积背景色空洞（截图非纯色/非黑）。
python3 - "$OUT/04-resize.png" <<'PY'
import sys
from PIL import Image
img = Image.open(sys.argv[1]).convert("RGB")
colors = img.getcolors(maxcolors=1 << 24) or []
print(f"  resize screenshot size={img.size} colors={len(colors)}")
if img.size[0] < 800 or len(colors) < 50:
    sys.exit("FAIL: resize 截图异常")
PY

# ---------------------------------------------------------------- 5. 主题切换
echo "== 5) 主题切换（config 热更新）=="
before_count=$(render_log | wc -l)
sed -i 's/^theme = .*/theme = "catppuccin-mocha"/' "$OUT/config.toml"
for _ in $(seq 1 100); do
    grep -Fq '已套用外部热更新' "$OUT/app.log" && break
    sleep 0.05
done
grep -Fq '已套用外部热更新' "$OUT/app.log" || fail "主题热更新未生效"
wait_render_after "$before_count" 100 || fail "主题切换后没有新的 render 行"
sleep 0.5
# 找配置生效后的整屏帧（中间可能夹着别的 partial）。
new_render_since "$before_count" | grep -q 'full=true' || fail "主题切换没有整屏重绘"
shot 05-theme
python3 - "$OUT/04-resize.png" "$OUT/05-theme.png" <<'PY'
import sys
from PIL import Image
before = Image.open(sys.argv[1]).convert("RGB")
after = Image.open(sys.argv[2]).convert("RGB")
# 只在终端内容区采样，避开 tabbar/设置等固定 UI。
def count(img, rgb, box):
    x0, y0, x1, y1 = box
    x1 = min(x1, img.size[0]); y1 = min(y1, img.size[1])
    return sum(1 for p in img.crop((x0, y0, x1, y1)).getdata() if p == rgb)
box = (40, 60, min(600, before.size[0]), min(500, before.size[1]))
tokyo = (0x1A, 0x1B, 0x26)
mocha = (0x1E, 0x1E, 0x2E)
b_tokyo, b_mocha = count(before, tokyo, box), count(before, mocha, box)
a_tokyo, a_mocha = count(after, tokyo, box), count(after, mocha, box)
print(f"  before tokyo={b_tokyo} mocha={b_mocha} | after tokyo={a_tokyo} mocha={a_mocha}")
if a_mocha < 500:
    sys.exit("FAIL: 主题切换后背景没有变成 catppuccin-mocha")
if a_tokyo > b_tokyo:
    sys.exit("FAIL: 主题切换后仍残留大量 tokyo-night 背景")
PY

# ---------------------------------------------------------------- 收尾断言
if grep -Fq '[panic]' "$OUT/app.log"; then
    fail "应用日志出现 panic"
fi
kill -0 "$APP_PID" 2>/dev/null || fail "应用已退出"

echo "PASS: 单行输出 full=false rows=1、搜索高亮局部重绘、滚动/resize/主题 full 重绘"
echo "截图: $OUT/{01-single-line,02-search-highlight,03-scroll,04-resize,05-theme}.png"
echo "日志: $LOG"
