#!/usr/bin/env bash
# SPDX-License-Identifier: GPL-3.0-only
# 终端菜单/查找 e2e：Match case/regex、Read Only、Reset/Reset and Clear、Save Contents。
#
# 在独立 headless Xvfb 上运行（默认 :119），只 kill 本脚本启动的 Xvfb/OpenBox/app，
# 不占用 :1、不启动 Xephyr、不 pkill 其他进程。
#
# 用法：
#   cargo build -p tervo-app --bin tervo --bin xwin --bin xclick --bin xkey
#   ./scripts/e2e_term_menu.sh
#
# 环境变量：E2E_BIN、E2E_OUT、E2E_DISPLAY、TERVO_PERF_DIR、E2E_XVFB_BIN。
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT=$PWD
OUT=${E2E_OUT:-$ROOT/.logs/term-menu}
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

DISPLAY_NUM=${E2E_DISPLAY:-:119}
export DISPLAY=$DISPLAY_NUM
export HOME=$OUT/home
export XMODIFIERS=@im=none
export SLINT_BACKEND=winit-software
export TERVO_CONFIG=$OUT/config.toml
export TERVO_LOG_DIR=$OUT/logs
export TERVO_KEYLOG=1

if xdpyinfo >/dev/null 2>&1; then
    echo "display $DISPLAY_NUM 已被占用；本脚本退出以避免干扰。" >&2
    exit 2
fi

XVFB_PID=""
OB_PID=""
APP_PID=""
CLIP_PID=""
cleanup() {
    [ -n "$APP_PID" ] && kill "$APP_PID" 2>/dev/null || true
    [ -n "$OB_PID" ] && kill "$OB_PID" 2>/dev/null || true
    [ -n "$XVFB_PID" ] && kill "$XVFB_PID" 2>/dev/null || true
    [ -n "$CLIP_PID" ] && kill "$CLIP_PID" 2>/dev/null || true
}
trap cleanup EXIT

cat > "$OUT/config.toml" <<'CFG'
theme = "tokyo-night"
font_size = 15
confirm_close = false
desktop_notifications = false
confirm_multiline_paste = false
CFG
cat > "$OUT/shell.sh" <<SHELL
#!/bin/sh
printf 'Alpha alpha\\nabc123\\nREADY\\n'
while IFS= read -r line; do
  printf '%s\\n' "\$line" >> "$OUT/input.txt"
  printf 'OUT:%s\\n' "\$line"
done
SHELL
chmod +x "$OUT/shell.sh"
rm -f "$OUT/input.txt" "$OUT/app.log"

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

"$BIN" --cols 80 --rows 24 --title Tervo --shell /bin/sh -- "$OUT/shell.sh" >"$OUT/app.log" 2>&1 &
APP_PID=$!
WIN=""
for _ in $(seq 1 200); do
    WIN=$(xwininfo -root -tree 2>/dev/null | grep -i '"tervo"' | head -1 | awk '{print $1}' || true)
    [ -n "$WIN" ] && break
    sleep 0.05
done
[ -n "$WIN" ] || { echo "找不到 Tervo 窗口；见 $OUT/app.log" >&2; tail -40 "$OUT/app.log" >&2; exit 2; }
"$XWIN" Tervo 20 20 >/dev/null 2>&1 || true
sleep 0.8

read -r ABS_X ABS_Y W H < <(xwininfo -id "$WIN" | awk '
    /Absolute upper-left X/{ax=$NF}
    /Absolute upper-left Y/{ay=$NF}
    /Width:/{w=$2}
    /Height:/{h=$2}
    END{print ax, ay, w, h}')
echo "  window=$WIN at $ABS_X,$ABS_Y ${W}x${H}"

key() { "$XKEY" "$@" >/dev/null 2>&1; sleep 0.16; }
click() { "$XCLICK" "$@" >/dev/null 2>&1; sleep 0.22; }
shot() {
    xwd -silent -id "$WIN" -out "$OUT/$1.xwd"
    ffmpeg -y -hide_banner -loglevel error -f xwd_pipe -i "$OUT/$1.xwd" "$OUT/$1.png"
    rm -f "$OUT/$1.xwd"
}
set_clipboard() { # <text>：xsel 在后台持有 CLIPBOARD selection
    printf '%s' "$1" | xsel -b -i >/dev/null 2>&1 &
    CLIP_PID=$!
    sleep 0.3
}
fail() {
    echo "FAIL: $*" >&2
    echo "--- app log tail ---" >&2
    tail -100 "$OUT/app.log" >&2
    exit 1
}
expect_log() { grep -Fq -- "$1" "$OUT/app.log" || fail "日志缺少: $1"; }

# 内容区坐标：窗口左侧 tabbar 高 32px。菜单 490px 高，24 行窗口内容区 528px，
# 在顶部附近右键时 12+490 < 528，不会翻转；改用 24 行是为了整张菜单都点得到。
PANE_X=$((ABS_X + 300))
PANE_Y_CONTENT=12
PANE_Y=$((ABS_Y + 32 + PANE_Y_CONTENT))
# 无链接时 13 项：padding 6 + 分组标题 22（Clipboard/Pane/View/Session）+ 行高 30。
# 项 index 0..12：Copy/Paste | SplitR/SplitD/Close/Broadcast | Find/SelectAll/Clear |
# ReadOnly/Reset/ResetClear/Save；该项之前经过的分组标题数：
# 0-1→1（Clipboard），2-5→2（+Pane），6-8→3（+View），9-12→4（+Session）。
menu_row_y() {
    local i=$1 groups
    if [ "$i" -le 1 ]; then groups=1
    elif [ "$i" -le 5 ]; then groups=2
    elif [ "$i" -le 8 ]; then groups=3
    else groups=4
    fi
    echo $((PANE_Y + 6 + 22 * groups + 30 * i + 15))
}
MENU_X=$((PANE_X + 50))
open_menu() {
    click "$PANE_X" "$PANE_Y" 3
    sleep 0.25
}
menu_pick() {
    open_menu
    click "$MENU_X" "$(menu_row_y "$1")"
    sleep 0.35
}
run_palette() {
    focus_pane
    key p --ctrl --shift
    sleep 0.35
    key "$1"
    sleep 0.35
    key --key enter
    sleep 0.5
}

# 等窗口/输入初始化完成。Xvfb+Openbox 首次映射偶尔会晚一帧，focus_pane 多重
# 申一次输入焦点，避免 e2e 偶发丢按键。
for _ in $(seq 1 100); do grep -Fq 'READY TIMING' "$OUT/app.log" 2>/dev/null && break; sleep 0.05; done
sleep 0.5
focus_pane() {
    "$XWIN" Tervo 20 20 >/dev/null 2>&1 || true
    sleep 0.12
    click "$PANE_X" $((ABS_Y + 32 + 200))
    sleep 0.15
}
focus_pane

echo "== 1) Find: Match case / regex / 非法 regex =="
key f --ctrl
sleep 0.3
key ALPHA
sleep 0.5
expect_log 'query="ALPHA" forward=true case=false regex=false hit=SearchHit'
shot find_nocase
# Match case 开关（搜索条第二行，窗口坐标 x=339..425/y=69..89）
click $((ABS_X + 339 + 43)) $((ABS_Y + 69 + 10))
sleep 0.5
expect_log 'find option match_case=true regex=false'
expect_log 'query="ALPHA" forward=true case=true regex=false hit=none'
shot find_case_on
# 再关掉 Match case，开 Match regex
click $((ABS_X + 339 + 43)) $((ABS_Y + 69 + 10))
sleep 0.3
click $((ABS_X + 339 + 86 + 6 + 43)) $((ABS_Y + 69 + 10))
sleep 0.4
expect_log 'find option match_case=false regex=true'
# 清空 ALPHA（5 个退格）后输入非法 regex
for _ in 1 2 3 4 5; do key --key backspace; done
key '['
sleep 0.5
expect_log 'error=invalid regex'
shot find_invalid
# 修正为 a.c：regex 模式命中 abc123（第 1 行）
key --key backspace
key a
key .
key c
sleep 0.5
expect_log 'query="a.c" forward=true case=false regex=true hit=SearchHit'
shot find_regex_hit
# Alt+R 关/开 regex 也应生效（GNOME Terminal 风格快捷键）
key r --alt
sleep 0.4
expect_log 'find option match_case=false regex=false'
key r --alt
sleep 0.4
expect_log 'find option match_case=false regex=true'
key --key escape
sleep 0.3

echo "== 2) Read Only：菜单切换 / 角标 / 键盘与粘贴不写 PTY =="
shot menu_open_pre
open_menu
shot menu_open
# Read Only 行下标 9
click "$MENU_X" "$(menu_row_y 9)"
sleep 0.4
expect_log '[ui] read-only pane=1 on=true'
shot readonly_on
# 点击 pane 聚焦后键入；UI 必须丢弃，不写 PTY。
focus_pane
key blocked
key --key enter
sleep 0.5
expect_log '[readonly] pane=1 丢弃按键'
if [ -f "$OUT/input.txt" ] && grep -Fq 'blocked' "$OUT/input.txt"; then fail "Read Only 开启时 blocked 仍写入 PTY"; fi
# 粘贴也必须被挡住（Ctrl+Shift+V）
set_clipboard 'pasted
'
key v --ctrl --shift
sleep 0.5
expect_log '[readonly] pane=1 丢弃粘贴'
if [ -f "$OUT/input.txt" ] && grep -Fq 'pasted' "$OUT/input.txt"; then fail "Read Only 开启时粘贴仍写入 PTY"; fi
# 菜单关闭
menu_pick 9
sleep 0.4
expect_log '[ui] read-only pane=1 on=false'
focus_pane
# 关闭后粘贴恢复
set_clipboard 'pasted
'
key v --ctrl --shift
sleep 0.7
grep -Fq 'pasted' "$OUT/input.txt" || fail "关闭 Read Only 后粘贴未恢复"
key allowed
key --key enter
sleep 0.7
grep -Fq 'allowed' "$OUT/input.txt" || fail "关闭 Read Only 后键盘输入未恢复"
shot readonly_off_after_input

echo "== 3) Save Contents：菜单入口 / 显式默认路径 / 错误提示 =="
# Session 组最后一项（index 12）：ReadOnly 9 / Reset 10 / ResetClear 11 / Save 12。
menu_pick 12
sleep 0.4
expect_log '[ui] save-contents open pane=1 path='
shot save_dialog
key --key enter
sleep 0.7
expect_log '[ui] save-contents ok pane=1 path='
SAVED=$(sed -n 's/.*\[ui\] save-contents ok pane=1 path=\(.*\) bytes=.*/\1/p' "$OUT/app.log" | tail -1)
[ -n "$SAVED" ] || fail "无法从日志解析保存路径"
[ -f "$SAVED" ] || fail "保存文件不存在: $SAVED"
grep -Fq 'abc123' "$SAVED" || fail "保存内容缺 abc123: $SAVED"
grep -Fq 'OUT:allowed' "$SAVED" || fail "保存内容缺 OUT:allowed: $SAVED"
echo "  saved=$SAVED bytes=$(wc -c < "$SAVED")"

echo "== 4) Reset / Reset and Clear（命令面板） =="
run_palette 'reset terminal'
expect_log '[ui] reset pane=1 clear-scrollback=false'
shot reset_plain
# Reset 后输入仍可用：再产生一行输出
focus_pane
key again
key --key enter
sleep 0.7
grep -Fq 'again' "$OUT/input.txt" || fail "Reset 后输入未恢复"
run_palette 'reset and clear'
expect_log '[ui] reset pane=1 clear-scrollback=true'
shot reset_and_clear
# 再输入一次确认 Reset and Clear 后会话未退出
focus_pane
key alive
key --key enter
sleep 0.7
grep -Fq 'alive' "$OUT/input.txt" || fail "Reset and Clear 后输入未恢复"

echo "== 5) 搜索/弹窗关闭后终端输入仍正常 =="
grep -Fq 'alive' "$OUT/input.txt" || fail "最后输入未落入 PTY"

echo "OK: $OUT"
echo "截图：find_nocase.png find_case_on.png find_invalid.png find_regex_hit.png menu_open.png readonly_on.png save_dialog.png reset_plain.png reset_and_clear.png"
