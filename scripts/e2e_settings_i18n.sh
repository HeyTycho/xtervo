#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
# Settings / 右键菜单中英双语 e2e：在 headless Xvfb 上启动 Tervo，覆盖
#   en/zh Appearance + Terminal、搜索、运行中切换、写回 config、外部热更新、
#   终端右键菜单、TabBar 菜单、小窗口、重启持久化、legacy modal。
# 同时做源码级断言：Settings/菜单所有静态文案必须走 Lang.t，en 侧不得含中文，
# zh 侧必须有中文且不能是未翻译的整句英文。
#
# 用法：
#   cargo build -p tervo-app --bin tervo --bin xwin --bin xclick --bin xkey
#   ./scripts/e2e_settings_i18n.sh
# 可用 TERVO_BIN_DIR 指定二进制目录（默认 target/debug），TERVO_PERF_DIR 指定
# Xvfb/Openbox 位置，E2E_DISPLAY 指定 display（默认 :126）。
# 依赖：Xvfb、Openbox、xwd、ffmpeg、xwininfo（仓库 .perf/ 已带前两者）。
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT=$PWD
OUT=${SETTINGS_I18N_OUT:-$ROOT/.logs/settings-i18n}
SHOTS=$OUT/shots
mkdir -p "$SHOTS" "$OUT/home" "$OUT/logs" /tmp/.X11-unix

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

DISPLAY_NUM=${E2E_DISPLAY:-:126}
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
LOG=""

start_app() {  # <tag> <lang> [legacy] [config-dir]
    local tag=$1 lang=$2 legacy=${3:-} cfgdir=${4:-}
    local cfg="$OUT/$tag.toml"
    if [ -n "$cfgdir" ]; then
        cfg=$cfgdir
    else
        # 每个 tag 使用干净配置，避免上一轮的语言残留影响断言。
        printf '%s\n' 'version = 1' "language = \"$lang\"" \
            'theme = "tokyo-night"' 'font_size = 15' 'confirm_close = false' > "$cfg"
    fi
    local log="$OUT/app_$tag.log"
    : >"$log"
    TERVO_SETTINGS_LEGACY="$legacy" TERVO_CONFIG="$cfg" TERVO_LOG_DIR="$OUT/logs" \
        "$BIN" --cols 120 --rows 32 --shell /bin/sh -- -c 'printf "https://example.com\n"; sleep 900' >"$log" 2>&1 &
    APP_PID=$!
    WIN=""
    for _ in $(seq 1 200); do
        WIN=$(xwininfo -root -tree 2>/dev/null | grep -i '"tervo"' | head -1 | awk '{print $1}' || true)
        [ -n "$WIN" ] && break
        sleep 0.05
    done
    [ -n "$WIN" ] || { echo "启动失败（$tag）：$log" >&2; cat "$log" >&2; exit 2; }
    "$XWIN" Tervo 20 20 >/dev/null 2>&1 || true
    sleep 0.8
    refresh_geom
    if [ "$W" != 1080 ] || [ "$H" != 736 ]; then
        echo "截图坐标基于 1080x736（cell 9x22 / 120x32），实际 ${W}x${H}" >&2
        exit 2
    fi
    LOG=$(ls -t "$OUT/logs"/tervo-*.log 2>/dev/null | head -1 || true)
    echo "  app=$tag lang=$lang legacy=${legacy:-none} window=${W}x${H} log=$LOG"
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
click_abs() { "$XCLICK" "$1" "$2" "${3:-1}" >/dev/null; }
open_settings() { click_rel "$((W - 118))" 16; sleep 0.7; }
close_settings() { "$XWIN" Tervo >/dev/null 2>&1 || true; "$XKEY" --key escape >/dev/null 2>&1 || true; sleep 0.5; }
click_nav() { click_rel 100 "$1"; sleep 0.55; }
# Appearance 顶部 Language 分段：左 English / 右 中文。
pick_lang() {  # en | zh（1080 宽）
    if [ "$1" = zh ]; then click_rel 850 196; else click_rel 450 196; fi
    sleep 0.8
}
pick_lang_small() {  # en | zh（760 宽，sidebar=164/content-w=556）
    if [ "$1" = zh ]; then click_rel 600 196; else click_rel 323 196; fi
    sleep 0.8
}
# 右键菜单在 winit-software 下偶尔晚一帧；截图过小则重开一次，避免把空终端当成功。
menu_shot() {  # <name> <x> <y>
    local name=$1 x=$2 y=$3 size
    for _ in 1 2 3; do
        click_rel "$x" "$y" 3
        sleep 1.0
        shot "$name"
        size=$(stat -c%s "$SHOTS/$name.png")
        [ "$size" -ge 20000 ] && return 0
        "$XWIN" Tervo >/dev/null 2>&1 || true
        "$XKEY" --key escape >/dev/null 2>&1 || true
        sleep 0.4
    done
    echo "右键菜单截图仍为空（$name, $size bytes）" >&2
    exit 2
}
open_search() { "$XWIN" Tervo >/dev/null 2>&1 || true; click_rel 880 56; sleep 0.35; }
assert_cfg_lang() {  # <file> <lang>
    grep -q "^language = \"$2\"" "$1" || {
        echo "config 未写回 language=$2：$1" >&2
        grep '^language' "$1" >&2 || true
        exit 2
    }
}
wait_log() {  # <needle> <timeout-ms>
    local needle=$1 timeout=${2:-4000} i
    for i in $(seq 1 $((timeout / 100))); do
        [ -n "$LOG" ] && grep -qF "$needle" "$LOG" 2>/dev/null && return 0
        sleep 0.1
    done
    echo "等待日志超时：$needle（$LOG）" >&2
    tail -40 "$LOG" >&2 || true
    exit 2
}

# ---------------------------------------------------------------- 源码级断言
echo "== 源码级双语断言 =="
python3 - "$ROOT/crates/app/ui/main.slint" "$ROOT/crates/app/src/main.rs" <<'PY'
import re, sys
slint = open(sys.argv[1], encoding='utf-8').read()
rust = open(sys.argv[2], encoding='utf-8').read()
has_cjk = lambda s: bool(re.search(r'[\u4e00-\u9fff]', s))
pairs = re.findall(r'Lang\.t\("((?:[^"\\]|\\.)*)",\s*"((?:[^"\\]|\\.)*)"\)', slint)
assert len(pairs) > 250, f'Lang.t 对太少：{len(pairs)}'
for en, zh in pairs:
    assert not has_cjk(en), f'en 文案混入中文: {en!r}'
    technical = en in ('ASCII DEL (0x7F)', 'Control-H (0x08)')
    assert has_cjk(zh) or technical, f'zh 文案没有中文: {en!r} -> {zh!r}'
    words = re.findall(r'[A-Za-z]{2,}', en)
    if len(words) >= 3 and not technical:
        assert zh != en, f'整句英文未翻译: {en!r}'
# Settings / 右键菜单 / TabBar / Profiles 的静态属性必须全部 Lang.t 包裹。
markers = [
    ('settings', '// ================= Settings =================', '// profiles-open 的遮罩'),
    ('context', '// ---------------- 右键菜单 ----------------', '// Profile 选择器菜单'),
    ('tabmenu', 'if root.tabmenu-open : Rectangle {', '// ================= Settings'),
    ('profiles', '// profiles-open 的遮罩', '// known_hosts 主机密钥预检：'),
]
allowed = {'', '×', '✕', '★', '☆', '✎', 'English', '中文', 'SSH', 'Telnet', 'Shell', 'SFTP', 'ASCII DEL', 'Control-H'}
for tag, start, end in markers:
    section = slint.split(start, 1)[1].split(end, 1)[0]
    stripped = re.sub(r'Lang\.t\("(?:[^"\\]|\\.)*",\s*"(?:[^"\\]|\\.)*"\)', '', section)
    for lineno, line in enumerate(stripped.splitlines(), 1):
        if line.lstrip().startswith('//'):
            continue
        for prop in ('text', 'label', 'description', 'placeholder', 'title'):
            m = re.search(rf'\b{prop}:\s*"([^"]*)"', line)
            if m:
                lit = m.group(1)
                assert lit in allowed or lit.startswith('\\{') or lit.startswith('Ctrl+') \
                    or lit.startswith('ESC [') or lit.startswith('/'), \
                    f'{tag} 第 {lineno} 行静态文案未走 Lang.t: {lit!r}'
# 中文页不能出现整句英文：SETTINGS_CATALOG 的 zh 列必须有中文。
for m in re.finditer(r'zh:\s*"([^"]*)"', rust):
    assert has_cjk(m.group(1)) or m.group(1) in ('Sixel 内联图片', 'iTerm2 IIP 图片'), \
        f'搜索索引 zh 未翻译: {m.group(1)!r}'
# 语言选项必须真的存在。
assert 'options: ["English", "中文"]' in slint
print(f'  OK: {len(pairs)} 组 Lang.t；Settings/右键/TabBar/Profiles 静态文案全部双语；en 无中文，zh 无未翻译整句')
PY

# ---------------------------------------------------------------- 运行中切换
echo "== 运行中切换 / 写回 config =="
start_app live en
EN_CFG=$OUT/live.toml
open_settings
shot 01_en_appearance
pick_lang zh
assert_cfg_lang "$EN_CFG" zh
wait_log "[ui] language=zh"
shot 02_zh_appearance
[ "$(sha256sum "$SHOTS/01_en_appearance.png" | cut -d' ' -f1)" != \
  "$(sha256sum "$SHOTS/02_zh_appearance.png" | cut -d' ' -f1)" ] || {
    echo "en/zh 截图完全相同：语言切换没有生效" >&2; exit 2;
}
# 中文 Terminal
click_nav 174
shot 03_zh_terminal
# 其余分类也截图，覆盖卡片标题/描述/字段 label/动态状态。
click_nav 210; shot 03b_zh_window
click_nav 280; shot 03c_zh_keys
click_nav 316; shot 03d_zh_profiles
# Profiles 面板 + 新建表单（Settings 内的模态静态文案）
click_rel 850 240; sleep 0.7; shot 03d2_zh_profiles_panel
click_rel 400 500; sleep 0.7; shot 03d3_zh_profile_form
click_rel 612 552; sleep 0.5   # 表单 Cancel
click_rel 694 500; sleep 0.5   # 面板 Close
click_nav 352; shot 03e_zh_plugins
click_nav 420; shot 03f_zh_vault
click_nav 456; shot 03g_zh_advanced
click_nav 138
# 主题下拉锚点（Language 卡片新增后仍在 Color scheme 下方）
click_rel 620 340; sleep 0.9; shot 03h_zh_theme_dropdown
"$XWIN" Tervo >/dev/null 2>&1 || true
"$XKEY" --key escape >/dev/null 2>&1 || true
sleep 0.4
# 切回英文
click_nav 138
pick_lang en
assert_cfg_lang "$EN_CFG" en
wait_log "[ui] language=en"
shot 04_en_appearance_restored
click_nav 174
shot 05_en_terminal
# 中文搜索（搜索索引/结果展示跟随语言）
click_nav 138
pick_lang zh
open_search
"$XKEY" sixel >/dev/null 2>&1 || true
sleep 0.7
shot 06_zh_search
"$XWIN" Tervo >/dev/null 2>&1 || true
"$XKEY" --key escape >/dev/null 2>&1 || true   # 清空搜索
sleep 0.3
# 英文搜索
pick_lang en
open_search
"$XKEY" sixel >/dev/null 2>&1 || true
sleep 0.7
shot 07_en_search
"$XWIN" Tervo >/dev/null 2>&1 || true
"$XKEY" --key escape >/dev/null 2>&1 || true
sleep 0.3

# ---------------------------------------------------------------- 右键菜单
echo "== 终端右键菜单 en/zh =="
pick_lang en
click_nav 138
close_settings
menu_shot 08_en_context_menu 30 43
"$XWIN" Tervo >/dev/null 2>&1 || true
"$XKEY" --key escape >/dev/null 2>&1 || true
sleep 0.4
open_settings
pick_lang zh
close_settings
menu_shot 09_zh_context_menu 30 43
"$XWIN" Tervo >/dev/null 2>&1 || true
"$XKEY" --key escape >/dev/null 2>&1 || true
sleep 0.4
# TabBar 菜单（顺带覆盖）
menu_shot 10_zh_tab_menu 600 16
"$XWIN" Tervo >/dev/null 2>&1 || true
"$XKEY" --key escape >/dev/null 2>&1 || true
sleep 0.4
open_settings
pick_lang en
close_settings
menu_shot 11_en_tab_menu 600 16
"$XWIN" Tervo >/dev/null 2>&1 || true
"$XKEY" --key escape >/dev/null 2>&1 || true
sleep 0.4

# ---------------------------------------------------------------- 外部热更新
echo "== 外部 config.toml 热更新 language =="
open_settings
pick_lang zh
python3 - "$EN_CFG" <<'PY'
import sys
p = sys.argv[1]
s = open(p, encoding='utf-8').read()
s = s.replace('language = "zh"', 'language = "en"', 1)
open(p, 'w', encoding='utf-8').write(s)
PY
wait_log "[config] 已套用外部热更新: language" 6000
assert_cfg_lang "$EN_CFG" en
sleep 0.8
shot 12_hot_reload_en

# ---------------------------------------------------------------- 小窗口
echo "== 小窗口 zh =="
pick_lang zh
# 拖右边/下边缩到约 760x520；恢复后坐标仍按刷新后的几何算。
"$XCLICK" "$((ABS_X + W - 1))" "$((ABS_Y + 300))" drag "$((ABS_X + 759))" "$((ABS_Y + 300))" >/dev/null
sleep 0.6
"$XCLICK" "$((ABS_X + 400))" "$((ABS_Y + H - 1))" drag "$((ABS_X + 400))" "$((ABS_Y + 519))" >/dev/null
sleep 0.8
refresh_geom
echo "  small window=${W}x${H}"
shot 13_zh_small
pick_lang_small en
shot 13b_en_small
pick_lang_small zh
shot 13c_zh_small_back
stop_app

# ---------------------------------------------------------------- 重启持久化
echo "== 重启后语言持久化 =="
start_app restart zh "" "$EN_CFG"
assert_cfg_lang "$EN_CFG" zh
open_settings
shot 14_zh_restart_persist
stop_app

# ---------------------------------------------------------------- legacy modal
echo "== legacy modal zh =="
start_app legacy zh 1
open_settings
shot 15_zh_legacy
stop_app

# ---------------------------------------------------------------- 结果断言
echo "== 结果断言 =="
for f in 01_en_appearance 02_zh_appearance 03_zh_terminal 03b_zh_window 03c_zh_keys \
         03d_zh_profiles 03d2_zh_profiles_panel 03d3_zh_profile_form 03e_zh_plugins 03f_zh_vault 03g_zh_advanced 03h_zh_theme_dropdown \
         04_en_appearance_restored 05_en_terminal 06_zh_search 07_en_search \
         08_en_context_menu 09_zh_context_menu 10_zh_tab_menu 11_en_tab_menu \
         12_hot_reload_en 13_zh_small 13b_en_small 13c_zh_small_back \
         14_zh_restart_persist 15_zh_legacy; do
    [ -s "$SHOTS/$f.png" ] || { echo "缺少截图 $f" >&2; exit 2; }
done
grep -qF "[ui] language=zh" "$OUT/app_live.log"
grep -qF "[ui] language=en" "$OUT/app_live.log"
grep -qF "已套用外部热更新: language" "$OUT/app_live.log"
echo "  OK: live en→zh→en、写回、外部热更新、右键/TabBar 菜单、小窗口、重启、legacy 全部截图"
echo "全部截图在 $SHOTS"
