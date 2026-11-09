#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
# Settings → Help 页 e2e：Xvfb 上打开 Settings，进入 SYSTEM 组最后的 Help，
# 覆盖 en/zh 截图、版本/commit、快捷键行、config/log 路径、复制诊断信息状态、
# 搜索 help/帮助 命中、窄窗滚动/宽窗铺满不溢出、缺少 xdg-open 时可见错误不崩溃。
#
# 用法：
#   cargo build -p tervo-app --bin tervo --bin xwin --bin xclick --bin xkey
#   ./scripts/e2e_settings_help.sh
# 环境变量：TERVO_BIN_DIR（默认 target/debug）、SETTINGS_HELP_OUT、
# E2E_DISPLAY（默认 :147）、TERVO_PERF_DIR、E2E_XVFB_BIN。
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT=$PWD
OUT=${SETTINGS_HELP_OUT:-$ROOT/.logs/settings-help}
SHOTS=$OUT/shots
mkdir -p "$SHOTS" "$OUT/home" "$OUT/logs" "$OUT/nobin" /tmp/.X11-unix

BIN_DIR=${TERVO_BIN_DIR:-$ROOT/target/debug}
BIN=$BIN_DIR/tervo
XWIN=$BIN_DIR/xwin
XCLICK=$BIN_DIR/xclick
XKEY=$BIN_DIR/xkey
for f in "$BIN" "$XWIN" "$XCLICK" "$XKEY"; do
    [ -x "$f" ] || {
        echo "缺少 $f；先 cargo build -p tervo-app --bin tervo --bin xwin --bin xclick --bin xkey" >&2
        exit 2
    }
done

COMMON=$(git rev-parse --git-common-dir)
MAIN_ROOT=$(cd "$(dirname "$COMMON")" && pwd)
PERF=${TERVO_PERF_DIR:-$MAIN_ROOT/.perf}
XVFB_BIN=${E2E_XVFB_BIN:-$PERF/xvfb/usr/bin/Xvfb}
[ -x "$XVFB_BIN" ] || XVFB_BIN=$(command -v Xvfb || true)
[ -x "$XVFB_BIN" ] || { echo "找不到 Xvfb；不会回退到 Xephyr" >&2; exit 2; }
OPENBOX=$PERF/openbox/usr/bin/openbox
OPENBOX_LIB=$PERF/openbox/usr/lib/x86_64-linux-gnu

DISPLAY_NUM=${E2E_DISPLAY:-:147}
export DISPLAY=$DISPLAY_NUM
export HOME=$OUT/home
export XMODIFIERS=@im=none
export SLINT_BACKEND=winit-software

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
LOG=""

start_app() {  # <tag> <lang> [extra-path]
    local tag=$1 lang=$2 path=${3:-}
    local cfg="$OUT/$tag.toml"
    printf '%s\n' 'version = 1' "language = \"$lang\"" \
        'theme = "tokyo-night"' 'font_size = 15' 'confirm_close = false' >"$cfg"
    local log="$OUT/app_$tag.log"
    : >"$log"
    if [ -n "$path" ]; then
        PATH="$path" TERVO_CONFIG="$cfg" TERVO_LOG_DIR="$OUT/logs" \
            "$BIN" --cols 120 --rows 32 --shell /bin/sh -- -c 'sleep 900' >"$log" 2>&1 &
    else
        TERVO_CONFIG="$cfg" TERVO_LOG_DIR="$OUT/logs" \
            "$BIN" --cols 120 --rows 32 --shell /bin/sh -- -c 'sleep 900' >"$log" 2>&1 &
    fi
    APP_PID=$!
    WIN=""
    for _ in $(seq 1 200); do
        WIN=$(xwininfo -root -tree 2>/dev/null | grep -i '"tervo"' | head -1 | awk '{print $1}' || true)
        [ -n "$WIN" ] && break
        sleep 0.05
    done
    [ -n "$WIN" ] || { echo "启动失败（$tag）：$log" >&2; cat "$log" >&2; exit 2; }
    "$XWIN" Tervo 20 20 >/dev/null 2>&1 || true
    sleep 0.9
    refresh_geom
    if [ "$W" != 1080 ] || [ "$H" != 736 ]; then
        echo "截图坐标基于 1080x736（cell 9x22 / 120x32），实际 ${W}x${H}" >&2
        exit 2
    fi
    LOG=$(ls -t "$OUT/logs"/tervo-*.log 2>/dev/null | head -1 || true)
    echo "  app=$tag lang=$lang window=${W}x${H} log=$LOG"
}

refresh_geom() {
    WIN=$(xwininfo -root -tree | grep -i '"tervo"' | head -1 | awk '{print $1}')
    ABS_X=$(xwininfo -id "$WIN" | awk '/Absolute upper-left X/{print $NF}')
    ABS_Y=$(xwininfo -id "$WIN" | awk '/Absolute upper-left Y/{print $NF}')
    W=$(xwininfo -id "$WIN" | awk '/Width:/{print $2}')
    H=$(xwininfo -id "$WIN" | awk '/Height:/{print $2}')
}

stop_app() {
    [ -n "$APP_PID" ] && kill "$APP_PID" 2>/dev/null || true
    wait "$APP_PID" 2>/dev/null || true
    APP_PID=""
    sleep 0.4
}

shot() {
    local name=$1
    xwd -silent -id "$WIN" -out "$SHOTS/$name.xwd"
    ffmpeg -y -hide_banner -loglevel error -f xwd_pipe -i "$SHOTS/$name.xwd" "$SHOTS/$name.png"
    rm -f "$SHOTS/$name.xwd"
    [ -s "$SHOTS/$name.png" ] || { echo "截图失败：$name" >&2; exit 2; }
    echo "  shot $SHOTS/$name.png"
}
click_rel() { "$XCLICK" "$((ABS_X + $1))" "$((ABS_Y + $2))" "${3:-1}" >/dev/null; }
open_settings() { click_rel "$((W - 118))" 16; sleep 0.7; }
close_settings() { "$XWIN" Tervo >/dev/null 2>&1 || true; "$XKEY" --key escape >/dev/null 2>&1 || true; sleep 0.4; }
click_nav() { click_rel 100 "$1"; sleep 0.55; }
open_search() { "$XWIN" Tervo >/dev/null 2>&1 || true; click_rel 880 56; sleep 0.35; }
click_result() { click_rel 300 152; sleep 0.9; }
scroll_down() { local n=${1:-5} i; for i in $(seq 1 "$n"); do click_rel 620 420 5; sleep 0.12; done; sleep 0.3; }

wait_log() {  # <needle> [timeout-ms]
    local needle=$1 timeout=${2:-5000} i
    for i in $(seq 1 $((timeout / 100))); do
        [ -n "$LOG" ] && grep -qF "$needle" "$LOG" 2>/dev/null && return 0
        sleep 0.1
    done
    echo "等待日志超时：$needle（$LOG）" >&2
    tail -60 "$LOG" >&2 || true
    exit 2
}

# 在 Help 页找到三个 ActionBtn（raise 底色 #24283b）的中心坐标。
find_action_buttons() {  # <png> -> "x y" per line
    python3 - "$1" <<'PY'
from collections import deque
from PIL import Image
import sys
im = Image.open(sys.argv[1]).convert('RGB')
px = im.load()
w, h = im.size
target = (36, 40, 59)  # Palette.raise (#24283b)
seen = set()
comps = []
for y in range(110, h - 4):
    for x in range(4, w - 4):
        if (x, y) in seen or px[x, y] != target:
            continue
        q = deque([(x, y)])
        seen.add((x, y))
        xs, ys = [], []
        while q:
            cx, cy = q.popleft()
            xs.append(cx); ys.append(cy)
            for nx, ny in ((cx-1, cy), (cx+1, cy), (cx, cy-1), (cx, cy+1)):
                if 0 <= nx < w and 0 <= ny < h and (nx, ny) not in seen and px[nx, ny] == target:
                    seen.add((nx, ny)); q.append((nx, ny))
        bw, bh = max(xs)-min(xs)+1, max(ys)-min(ys)+1
        if 36 <= bw <= 420 and 18 <= bh <= 36 and len(xs) >= 300:
            comps.append((min(xs), max(xs), min(ys), max(ys), len(xs)))
# 同一行（y 重叠）的按钮按 x 排序。
comps.sort(key=lambda c: (c[2] // 8, c[0]))
for x0, x1, y0, y1, n in comps:
    print((x0 + x1) // 2, (y0 + y1) // 2)
PY
}

# 内容卡片必须铺满 content-w，但不越过右边界（窄/宽窗都不溢出）。
check_content_fit() {  # <png> <W> <label>
    python3 - "$1" "$2" "$3" <<'PY'
from PIL import Image
import sys
path, W, label = sys.argv[1], int(sys.argv[2]), sys.argv[3]
im = Image.open(path).convert('RGB')
px = im.load()
iw, ih = im.size
surface = (22, 22, 30)  # Palette.surface (#16161e) = SettingsCard 背景
sidebar = 220 if W >= 900 else min(164, max(156, int(W * 0.32)))
content_w = W - sidebar - 40
content_x = sidebar + 20
cols = []
for x in range(sidebar + 2, iw):
    n = sum(1 for y in range(104, ih - 8) if px[x, y] == surface)
    if n >= 40:
        cols.append(x)
if not cols:
    print(f'  [{label}] no card surface found'); sys.exit(1)
x0, x1 = min(cols), max(cols)
width = x1 - x0 + 1
print(f'  [{label}] card x={x0}..{x1} width={width} expected content_x={content_x} content_w={content_w} window={W}')
ok = abs(x0 - (content_x + 1)) <= 3 and abs(width - (content_w - 2)) <= 8
ok = ok and x1 < content_x + content_w + 1
if not ok:
    print(f'  [{label}] card overflow / not filling content area'); sys.exit(1)
PY
}

# 窄窗滚动：内容区必须实际发生位移（Flickable 可滚动）。
check_scrolled() {  # <before> <after>
    python3 - "$1" "$2" <<'PY'
from PIL import Image, ImageChops
import sys
a = Image.open(sys.argv[1]).convert('RGB')
b = Image.open(sys.argv[2]).convert('RGB')
box = (170, 100, a.size[0] - 10, a.size[1] - 10)
diff = ImageChops.difference(a.crop(box), b.crop(box))
changed = sum(1 for p in diff.getdata() if max(p) > 8)
print(f'  scrolled content pixels={changed}')
sys.exit(0 if changed > 500 else 1)
PY
}

# ---------------------------------------------------------------- 源码级断言
echo "== 源码级：Help 图标/双语/快捷键/搜索索引 =="
python3 - "$ROOT/crates/app/ui/main.slint" "$ROOT/crates/app/src/main.rs" <<'PY'
import re, sys
slint = open(sys.argv[1], encoding='utf-8').read()
rust = open(sys.argv[2], encoding='utf-8').read()
has_cjk = lambda s: bool(re.search(r'[\u4e00-\u9fff]', s))

assert re.search(r'enum SettingsIconKind \{[^}]*\bhelp,', slint, re.S), 'SettingsIconKind.help 缺失'
icon = slint.split('// Help：圆形问号', 1)[1].split('// Gear：', 1)[0]
assert 'commands:' in icon and 'border-radius: 6.4px' in icon, '圆形问号不是手绘 Path/Rectangle'
assert 'Text {' not in icon, 'Help 图标不能依赖文字/emoji'

sec = slint.split('// ---------------- Help ----------------', 1)[1].split('// ---------------- 搜索结果浮层', 1)[0]
pairs = re.findall(r'Lang\.t\("((?:[^"\\]|\\.)*)",\s*"((?:[^"\\]|\\.)*)"\)', sec)
assert len(pairs) >= 30, f'Help 页 Lang.t 对太少：{len(pairs)}'
for en, zh in pairs:
    assert not has_cjk(en), f'Help en 文案混入中文: {en!r}'
    assert has_cjk(zh), f'Help zh 文案没有中文: {en!r} -> {zh!r}'
for key in ('Ctrl+T', 'Ctrl+W', 'Ctrl+F', 'Ctrl+Shift+C', 'Ctrl+Shift+V',
            'Ctrl+Shift+E', 'Ctrl+Shift+D', 'F11'):
    assert key in sec, f'快捷键行缺失: {key}'
for prop in ('title', 'description', 'label', 'action'):
    for m in re.finditer(rf'\b{prop}:\s*"([^"]*)"', sec):
        raise AssertionError(f'Help 静态文案未走 Lang.t: {prop}={m.group(1)!r}')
assert 'root.help-config-path' in sec and 'root.help-log-path' in sec, '路径未展示'
assert 'root.help-version' in sec and 'root.help-commit' in sec, '版本/commit 未展示'
assert 'root.help-built' in sec and 'root.help-target' in sec, '构建时间/target 未展示'
assert 'root.settings-help-copy-diagnostics' in sec and 'root.settings-help-open' in sec, '按钮回调缺失'
assert 'help-status' in sec, '复制/打开状态文案缺失'

catalog = rust.split('const SETTINGS_CATALOG', 1)[1].split('];', 1)[0]
help_entries = [l for l in catalog.splitlines() if 'cat: 8' in l]
assert len(help_entries) == 4, f'Help 搜索索引应有 4 条，实际 {len(help_entries)}'
for line in help_entries:
    assert 'help' in line.lower() and '帮助' in line, line
assert 'on_settings_help_open' in rust and 'on_settings_help_copy_diagnostics' in rust
assert 'refresh_help_info' in rust and 'open_help_directory' in rust
assert 'open_with_xdg_observed' in rust and 'open_with_xdg_observed(dir.into_os_string()' in rust, 'Help 未复用 xdg-open helper'
assert 'Command::new("xdg-open")' in rust and '.arg(&target)' in rust, 'xdg-open 必须单参数、shell-safe'
assert '.arg(format!' not in rust, 'xdg-open 不得拼接 shell 参数'
print(f'  OK: Help 图标手绘；{len(pairs)} 组 Lang.t；9 条快捷键；4 条搜索索引；回调已接线')
PY

# ---------------------------------------------------------------- 版本基线
CARGO_VERSION=$(awk -F'"' '/^version = /{print $2; exit}' "$ROOT/Cargo.toml")
HEAD_SHORT=$(git rev-parse --short=12 HEAD)
echo "  expected version=$CARGO_VERSION head=$HEAD_SHORT"

# ---------------------------------------------------------------- en 场景
echo "== en：Help 页面 / 路径 / 复制诊断 / 搜索 =="
start_app en en
open_settings
click_nav 492
wait_log "[ui] settings category=8"
wait_log "[help] info version=$CARGO_VERSION"
grep -Eq '\[help\] info .*commit=[0-9a-f]{7,12}' "$LOG" || {
    echo "Help 版本信息缺少 commit" >&2; tail -30 "$LOG" >&2; exit 2;
}
grep -q "config=$OUT " "$LOG" || { echo "Help config 路径未下发" >&2; exit 2; }
grep -q "logs=$OUT/logs" "$LOG" || { echo "Help logs 路径未下发" >&2; exit 2; }
shot 01_en_help_top

# 搜索 help：4 条命中（含 Help 分类），点击第一条跳回 Help。
open_search
"$XKEY" help >/dev/null 2>&1 || true
sleep 0.7
wait_log '[ui] settings filter q="help" hits=4'
shot 02_en_search_help
click_result
wait_log '[ui] settings goto cat=8'
shot 03_en_search_jump

# 搜索 diagnostic 跳到 paths 卡片，三个按钮应同时可见。
open_search
"$XKEY" diagnostic >/dev/null 2>&1 || true
sleep 0.7
wait_log '[ui] settings filter q="diagnostic" hits=1'
click_result
wait_log '[ui] settings jump section=paths'
shot 04_en_help_paths
mapfile -t BTNS < <(find_action_buttons "$SHOTS/04_en_help_paths.png")
[ "${#BTNS[@]}" -eq 3 ] || {
    echo "期望 3 个 Help 按钮，实际 ${#BTNS[@]}: ${BTNS[*]:-}" >&2; exit 2;
}
echo "  buttons: ${BTNS[*]}"
read -r BX BY <<<"${BTNS[0]}"
click_rel "$BX" "$BY"; sleep 0.5
wait_log '[help] open config path='
read -r BX BY <<<"${BTNS[1]}"
click_rel "$BX" "$BY"; sleep 0.5
wait_log '[help] open logs path='
read -r BX BY <<<"${BTNS[2]}"
click_rel "$BX" "$BY"; sleep 0.8
wait_log '[help] copy diagnostics ok=true'
shot 05_en_help_diagnostics_copied
kill -0 "$APP_PID" 2>/dev/null || { echo "点击按钮后应用退出" >&2; exit 2; }

# 版本卡片截图（搜索 version 跳转，验证搜索锚点）。
open_search
"$XKEY" version >/dev/null 2>&1 || true
sleep 0.7
wait_log '[ui] settings filter q="version"'
click_result
wait_log '[ui] settings jump section=version'
shot 06_en_help_version
grep -q "target=" "$LOG"

# 窄窗：内容随窗口缩放、右边界不溢出、滚动生效。
"$XWIN" Tervo 20 20 760 520 >/dev/null 2>&1 || true
sleep 0.9
refresh_geom
shot 07_en_help_narrow
check_content_fit "$SHOTS/07_en_help_narrow.png" "$W" "narrow 760x520"
scroll_down 5
shot 08_en_help_narrow_scrolled
check_scrolled "$SHOTS/07_en_help_narrow.png" "$SHOTS/08_en_help_narrow_scrolled.png" || {
    echo "窄窗 Help 滚动无变化" >&2; exit 2;
}

# 宽窗：卡片撑满内容区。
"$XWIN" Tervo 20 20 1400 900 >/dev/null 2>&1 || true
sleep 0.9
refresh_geom
shot 09_en_help_wide
check_content_fit "$SHOTS/09_en_help_wide.png" "$W" "wide 1400x900"
stop_app

# ---------------------------------------------------------------- zh 场景
echo "== zh：Help 页面 / 搜索帮助 =="
start_app zh zh
open_settings
click_nav 492
wait_log "[ui] settings category=8"
wait_log "[help] info version=$CARGO_VERSION"
shot 10_zh_help_top

# 中文搜索：剪贴板写入“帮助”，Ctrl+V 粘进搜索框（xkey 只支持 ASCII keysym）。
printf '帮助' | xsel --clipboard --input
sleep 0.3
open_search
"$XKEY" --ctrl v >/dev/null 2>&1 || true
sleep 0.9
if ! grep -qF '[ui] settings filter q="帮助"' "$LOG"; then
    # 回退：用 xsel 直接填 PRIMARY 后中键粘贴，仍失败则明确报错。
    click_rel 880 56
    printf '帮助' | xsel --primary --input
    "$XCLICK" "$((ABS_X + 880))" "$((ABS_Y + 56))" 2 >/dev/null 2>&1 || true
    sleep 0.9
fi
wait_log '[ui] settings filter q="帮助" hits=4'
shot 11_zh_search_help
click_result
wait_log '[ui] settings goto cat=8'
shot 12_zh_search_jump

open_search
"$XKEY" diagnostic >/dev/null 2>&1 || true
sleep 0.7
wait_log '[ui] settings filter q="diagnostic" hits=1'
click_result
wait_log '[ui] settings jump section=paths'
shot 13_zh_help_paths
mapfile -t BTNS < <(find_action_buttons "$SHOTS/13_zh_help_paths.png")
[ "${#BTNS[@]}" -eq 3 ] || { echo "zh 期望 3 个 Help 按钮，实际 ${#BTNS[@]}" >&2; exit 2; }
read -r BX BY <<<"${BTNS[2]}"
click_rel "$BX" "$BY"; sleep 0.8
wait_log '[help] copy diagnostics ok=true'
shot 14_zh_help_diagnostics_copied
stop_app

# ---------------------------------------------------------------- 无 xdg-open
echo "== 无 xdg-open：可见错误且不 panic =="
start_app noxdg en "$OUT/nobin"
open_settings
click_nav 492
wait_log "[ui] settings category=8"
open_search
"$XKEY" diagnostic >/dev/null 2>&1 || true
sleep 0.7
wait_log '[ui] settings filter q="diagnostic" hits=1'
click_result
wait_log '[ui] settings jump section=paths'
shot 15_en_no_xdg_paths
mapfile -t BTNS < <(find_action_buttons "$SHOTS/15_en_no_xdg_paths.png")
[ "${#BTNS[@]}" -eq 3 ] || { echo "无 xdg 场景按钮数=${#BTNS[@]}" >&2; exit 2; }
read -r BX BY <<<"${BTNS[0]}"
click_rel "$BX" "$BY"; sleep 0.8
wait_log '[help] open config failed'
kill -0 "$APP_PID" 2>/dev/null || { echo "缺少 xdg-open 时应用崩溃" >&2; exit 2; }
shot 16_en_no_xdg_error
stop_app

echo "PASS: Settings → Help（en/zh、路径、复制诊断、搜索、窄/宽窗、无 xdg-open）"
echo "截图：$SHOTS"
